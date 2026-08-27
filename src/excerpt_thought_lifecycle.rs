//! Authoritative Excerpt and Thought lifecycle and projection seam.
//!
//! Callers submit complete intent and adopt the returned SQLite projection.
//! Anchor identity, Legacy Excerpt preservation, search visibility, counts,
//! transactions, and maintenance fencing remain hidden here.

mod store;

use chrono::Utc;
use rusqlite::Connection;

use crate::db::Db;
use crate::library_projection_revision::{
    self, ProjectionFamily, ProjectionImpact, ProjectionStamp,
};
use crate::model::TextAnchor;

const MAX_EXCERPT_BYTES: usize = 256 * 1024;
const MAX_THOUGHT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ProjectionScope {
    Article(i64),
    Library,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExcerptCapture {
    pub(crate) article_id: i64,
    pub(crate) selected_text: String,
    pub(crate) anchor: TextAnchor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExcerptTarget {
    Existing(i64),
    Captured(ExcerptCapture),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExcerptThoughtChange {
    EnsureExcerpt {
        capture: ExcerptCapture,
    },
    PutThought {
        target: ExcerptTarget,
        content: String,
    },
    RemoveThought {
        excerpt_id: i64,
    },
    DeleteExcerpt {
        excerpt_id: i64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExcerptIdentityKind {
    Managed,
    Legacy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExcerptResolution {
    Resolved { char_start: usize, char_end: usize },
    Unresolved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThoughtView {
    pub(crate) content: String,
    pub(crate) updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArticleOrigin {
    Feed,
    WebClipping,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArticleNavigation {
    pub(crate) article_id: i64,
    pub(crate) feed_id: i64,
    pub(crate) title: Option<String>,
    pub(crate) url: Option<String>,
    pub(crate) origin: ArticleOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExcerptView {
    pub(crate) id: i64,
    pub(crate) article_id: i64,
    pub(crate) selected_text: String,
    pub(crate) anchor: TextAnchor,
    pub(crate) thought: Option<ThoughtView>,
    pub(crate) resolution: ExcerptResolution,
    pub(crate) identity_kind: ExcerptIdentityKind,
    pub(crate) source: ArticleNavigation,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
    lifecycle_identity: Option<Vec<u8>>,
}

impl ExcerptView {
    pub(crate) fn as_article_selection(&self) -> crate::model::ArticleSelection {
        crate::model::ArticleSelection {
            id: self.id,
            article_id: self.article_id,
            selected_text: self.selected_text.clone(),
            start_offset: self.anchor.start_offset,
            end_offset: self.anchor.end_offset,
            anchor_prefix: self.anchor.prefix.clone(),
            anchor_suffix: self.anchor.suffix.clone(),
            comment: self.thought.as_ref().map(|thought| thought.content.clone()),
            is_favorite: true,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ExcerptThoughtCounts {
    pub(crate) library_excerpts: usize,
    pub(crate) library_thoughts: usize,
    pub(crate) scope_excerpts: usize,
    pub(crate) scope_thoughts: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExcerptThoughtProjection {
    pub(crate) stamp: ProjectionStamp,
    pub(crate) scope: ProjectionScope,
    pub(crate) excerpts: Vec<ExcerptView>,
    pub(crate) counts: ExcerptThoughtCounts,
}

impl ExcerptThoughtProjection {
    pub(crate) fn match_capture(&self, capture: &ExcerptCapture) -> Option<&ExcerptView> {
        let identity = store::capture_identity(capture)?;
        self.excerpts.iter().find(|excerpt| {
            excerpt.article_id == capture.article_id
                && excerpt.lifecycle_identity.as_deref() == Some(identity.as_slice())
        })
    }

    pub(crate) fn excerpt(&self, excerpt_id: i64) -> Option<&ExcerptView> {
        self.excerpts
            .iter()
            .find(|excerpt| excerpt.id == excerpt_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeDisposition {
    Created,
    Changed,
    Unchanged,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApplyOutcome {
    pub(crate) disposition: ChangeDisposition,
    pub(crate) affected_excerpt_id: i64,
    pub(crate) projection: ExcerptThoughtProjection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureKind {
    Input,
    NotFound,
    Maintenance,
    Storage,
    Invariant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleOperation {
    ValidateInput,
    Project,
    BeginTransaction,
    ApplyChange,
    ReloadProjection,
    Commit,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{user_message}")]
pub(crate) struct LifecycleFailure {
    pub(crate) kind: FailureKind,
    pub(crate) user_message: String,
    pub(crate) technical_detail: String,
    pub(crate) operation: Option<LifecycleOperation>,
}

impl LifecycleFailure {
    fn input(message: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Input,
            user_message: message.into(),
            technical_detail: bounded_detail(detail.into()),
            operation: Some(LifecycleOperation::ValidateInput),
        }
    }

    fn from_store(operation: LifecycleOperation, failure: store::StoreFailure) -> Self {
        let (kind, user_message) = match &failure {
            store::StoreFailure::NotFound(_) => (FailureKind::NotFound, "摘录或原文已经不存在"),
            store::StoreFailure::InvalidCapture(_) => (FailureKind::Input, "摘录位置无效"),
            store::StoreFailure::Invariant(_) => {
                (FailureKind::Invariant, "摘录资料状态不一致，操作未执行")
            }
            store::StoreFailure::Sqlite(error)
                if error.to_string().contains("MAINTENANCE_IN_PROGRESS")
                    || error.to_string().contains("STALE_LIBRARY_EPOCH") =>
            {
                (FailureKind::Maintenance, "资料维护期间不能修改摘录与想法")
            }
            store::StoreFailure::Sqlite(_) => (FailureKind::Storage, "摘录与想法操作失败"),
        };
        Self {
            kind,
            user_message: user_message.into(),
            technical_detail: bounded_detail(failure.to_string()),
            operation: Some(operation),
        }
    }

    fn storage(operation: LifecycleOperation, error: impl std::fmt::Display) -> Self {
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
                "资料维护期间不能修改摘录与想法".into()
            } else {
                "摘录与想法操作失败".into()
            },
            technical_detail: bounded_detail(detail),
            operation: Some(operation),
        }
    }
}

fn bounded_detail(mut detail: String) -> String {
    const MAX_DETAIL_CHARS: usize = 2_000;
    if detail.chars().count() > MAX_DETAIL_CHARS {
        detail = detail.chars().take(MAX_DETAIL_CHARS).collect();
        detail.push('…');
    }
    detail
}

pub(crate) trait Clock {
    fn now(&self) -> i64;
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        Utc::now().timestamp()
    }
}

pub(crate) static SYSTEM_CLOCK: SystemClock = SystemClock;

pub(crate) struct ExcerptThoughtLifecycle<'db, 'clock> {
    db: &'db Db,
    clock: &'clock dyn Clock,
}

impl<'db, 'clock> ExcerptThoughtLifecycle<'db, 'clock> {
    pub(crate) fn new(db: &'db Db, clock: &'clock dyn Clock) -> Self {
        Self { db, clock }
    }

    pub(crate) fn project(
        &self,
        scope: ProjectionScope,
    ) -> Result<ExcerptThoughtProjection, LifecycleFailure> {
        validate_scope(scope)?;
        let tx = self
            .db
            .conn
            .unchecked_transaction()
            .map_err(|error| LifecycleFailure::storage(LifecycleOperation::Project, error))?;
        let revision = library_projection_revision::read_family(&tx, ProjectionFamily::Excerpt)
            .map_err(|error| LifecycleFailure::storage(LifecycleOperation::Project, error))?;
        let projection = store::project(
            &tx,
            scope,
            ProjectionStamp {
                generation: self.db.library_generation(),
                revision,
            },
        )
        .map_err(|failure| LifecycleFailure::from_store(LifecycleOperation::Project, failure))?;
        tx.commit()
            .map_err(|error| LifecycleFailure::storage(LifecycleOperation::Project, error))?;
        Ok(projection)
    }

    pub(crate) fn apply(
        &self,
        change: ExcerptThoughtChange,
        refresh_scope: ProjectionScope,
    ) -> Result<ApplyOutcome, LifecycleFailure> {
        let change = validate_change(change)?;
        validate_scope(refresh_scope)?;
        let tx = self.db.fenced_transaction().map_err(|error| {
            LifecycleFailure::storage(LifecycleOperation::BeginTransaction, error)
        })?;
        let changed = store::apply_change(&tx, &change, self.clock.now()).map_err(|failure| {
            LifecycleFailure::from_store(LifecycleOperation::ApplyChange, failure)
        })?;
        let impact = if changed.disposition == ChangeDisposition::Unchanged {
            ProjectionImpact::none()
        } else {
            ProjectionImpact::excerpt()
        };
        let revisions = library_projection_revision::record(&tx, impact)
            .map_err(|error| LifecycleFailure::storage(LifecycleOperation::ApplyChange, error))?;
        let projection = store::project(
            &tx,
            refresh_scope,
            ProjectionStamp {
                generation: self.db.library_generation(),
                revision: revisions.excerpt,
            },
        )
        .map_err(|failure| {
            LifecycleFailure::from_store(LifecycleOperation::ReloadProjection, failure)
        })?;
        tx.commit()
            .map_err(|error| LifecycleFailure::storage(LifecycleOperation::Commit, error))?;
        Ok(ApplyOutcome {
            disposition: changed.disposition,
            affected_excerpt_id: changed.excerpt_id,
            projection,
        })
    }
}

fn validate_scope(scope: ProjectionScope) -> Result<(), LifecycleFailure> {
    if let ProjectionScope::Article(article_id) = scope
        && article_id <= 0
    {
        return Err(LifecycleFailure::input(
            "文章标识无效",
            format!("INVALID_ARTICLE_ID: {article_id}"),
        ));
    }
    Ok(())
}

fn validate_change(change: ExcerptThoughtChange) -> Result<ExcerptThoughtChange, LifecycleFailure> {
    match change {
        ExcerptThoughtChange::EnsureExcerpt { capture } => {
            Ok(ExcerptThoughtChange::EnsureExcerpt {
                capture: validate_capture(capture)?,
            })
        }
        ExcerptThoughtChange::PutThought { target, content } => {
            let target = match target {
                ExcerptTarget::Existing(excerpt_id) => {
                    validate_excerpt_id(excerpt_id)?;
                    ExcerptTarget::Existing(excerpt_id)
                }
                ExcerptTarget::Captured(capture) => {
                    ExcerptTarget::Captured(validate_capture(capture)?)
                }
            };
            let content = content.replace("\r\n", "\n").replace('\r', "\n");
            if content.trim().is_empty() {
                return Err(LifecycleFailure::input("想法内容不能为空", "EMPTY_THOUGHT"));
            }
            if content.len() > MAX_THOUGHT_BYTES {
                return Err(LifecycleFailure::input(
                    "想法内容过长",
                    format!("THOUGHT_TOO_LARGE: {} > {MAX_THOUGHT_BYTES}", content.len()),
                ));
            }
            Ok(ExcerptThoughtChange::PutThought { target, content })
        }
        ExcerptThoughtChange::RemoveThought { excerpt_id } => {
            validate_excerpt_id(excerpt_id)?;
            Ok(ExcerptThoughtChange::RemoveThought { excerpt_id })
        }
        ExcerptThoughtChange::DeleteExcerpt { excerpt_id } => {
            validate_excerpt_id(excerpt_id)?;
            Ok(ExcerptThoughtChange::DeleteExcerpt { excerpt_id })
        }
    }
}

fn validate_capture(capture: ExcerptCapture) -> Result<ExcerptCapture, LifecycleFailure> {
    if capture.article_id <= 0 {
        return Err(LifecycleFailure::input(
            "文章标识无效",
            format!("INVALID_ARTICLE_ID: {}", capture.article_id),
        ));
    }
    if capture.selected_text.trim().is_empty() {
        return Err(LifecycleFailure::input(
            "选中的文字不能为空",
            "EMPTY_EXCERPT",
        ));
    }
    if capture.selected_text.len() > MAX_EXCERPT_BYTES {
        return Err(LifecycleFailure::input(
            "选中的文字过长",
            format!(
                "EXCERPT_TOO_LARGE: {} > {MAX_EXCERPT_BYTES}",
                capture.selected_text.len()
            ),
        ));
    }
    match (capture.anchor.start_offset, capture.anchor.end_offset) {
        (Some(start), Some(end)) if start >= 0 && end > start => {}
        _ => {
            return Err(LifecycleFailure::input(
                "摘录位置无效",
                "INVALID_EXCERPT_ANCHOR",
            ));
        }
    }
    Ok(capture)
}

fn validate_excerpt_id(excerpt_id: i64) -> Result<(), LifecycleFailure> {
    if excerpt_id <= 0 {
        return Err(LifecycleFailure::input(
            "摘录标识无效",
            format!("INVALID_EXCERPT_ID: {excerpt_id}"),
        ));
    }
    Ok(())
}

pub(crate) fn migrate_to_v7(conn: &Connection) -> anyhow::Result<()> {
    store::migrate_to_v7(conn)
}

pub(crate) fn verify_schema_v7(conn: &Connection) -> anyhow::Result<()> {
    store::verify_schema_v7(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[derive(Debug)]
    struct FixedClock(i64);

    impl Clock for FixedClock {
        fn now(&self) -> i64 {
            self.0
        }
    }

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::schema_evolution::evolve(&conn).unwrap();
        Db {
            conn,
            path: None,
            _maintenance_fence: None,
        }
    }

    fn seed_article(db: &Db, content: &str) -> i64 {
        db.conn
            .execute(
                "INSERT INTO feeds(url,title,next_fetch) VALUES('https://example.test/feed','Feed',0)",
                [],
            )
            .unwrap();
        let feed_id = db.conn.last_insert_rowid();
        db.conn
            .execute(
                "INSERT INTO articles(feed_id,entry_id,url,title,content,fetched_at)
                 VALUES(?1,'entry','https://example.test/article','Article',?2,1)",
                rusqlite::params![feed_id, content],
            )
            .unwrap();
        db.conn.last_insert_rowid()
    }

    fn capture(article_id: i64, document: &str, quote: &str) -> ExcerptCapture {
        let byte_start = document.find(quote).unwrap();
        let start = document[..byte_start].chars().count();
        let end = start + quote.chars().count();
        ExcerptCapture {
            article_id,
            selected_text: quote.into(),
            anchor: TextAnchor::capture(document, start, end, 8),
        }
    }

    #[test]
    fn ensure_excerpt_reuses_identity_without_moving_sort_time() {
        let db = test_db();
        let document = "before durable quote after";
        let article_id = seed_article(&db, document);
        let capture = capture(article_id, document, "durable quote");
        let first = ExcerptThoughtLifecycle::new(&db, &FixedClock(100))
            .apply(
                ExcerptThoughtChange::EnsureExcerpt {
                    capture: capture.clone(),
                },
                ProjectionScope::Article(article_id),
            )
            .unwrap();
        assert_eq!(first.disposition, ChangeDisposition::Created);
        assert!(first.projection.stamp.revision > 0);
        let second = ExcerptThoughtLifecycle::new(&db, &FixedClock(200))
            .apply(
                ExcerptThoughtChange::EnsureExcerpt { capture },
                ProjectionScope::Article(article_id),
            )
            .unwrap();
        assert_eq!(second.disposition, ChangeDisposition::Unchanged);
        assert_eq!(second.projection.stamp, first.projection.stamp);
        assert_eq!(second.affected_excerpt_id, first.affected_excerpt_id);
        assert_eq!(second.projection.excerpts.len(), 1);
        assert_eq!(second.projection.excerpts[0].updated_at, 100);
        assert_eq!(second.projection.counts.library_excerpts, 1);
    }

    #[test]
    fn captured_thought_creates_excerpt_and_supports_replace_and_remove() {
        let db = test_db();
        let document = "before quote after";
        let article_id = seed_article(&db, document);
        let capture = capture(article_id, document, "quote");
        let created = ExcerptThoughtLifecycle::new(&db, &FixedClock(100))
            .apply(
                ExcerptThoughtChange::PutThought {
                    target: ExcerptTarget::Captured(capture.clone()),
                    content: "first\r\nthought".into(),
                },
                ProjectionScope::Article(article_id),
            )
            .unwrap();
        assert_eq!(created.disposition, ChangeDisposition::Created);
        assert_eq!(created.projection.counts.library_excerpts, 1);
        assert_eq!(created.projection.counts.library_thoughts, 1);
        assert_eq!(
            created.projection.excerpts[0]
                .thought
                .as_ref()
                .unwrap()
                .content,
            "first\nthought"
        );
        assert!(created.projection.match_capture(&capture).is_some());

        let excerpt_id = created.affected_excerpt_id;
        let replaced = ExcerptThoughtLifecycle::new(&db, &FixedClock(200))
            .apply(
                ExcerptThoughtChange::PutThought {
                    target: ExcerptTarget::Existing(excerpt_id),
                    content: "replacement".into(),
                },
                ProjectionScope::Library,
            )
            .unwrap();
        assert_eq!(replaced.disposition, ChangeDisposition::Changed);
        assert_eq!(replaced.projection.excerpts[0].updated_at, 200);

        let unchanged = ExcerptThoughtLifecycle::new(&db, &FixedClock(300))
            .apply(
                ExcerptThoughtChange::PutThought {
                    target: ExcerptTarget::Existing(excerpt_id),
                    content: "replacement".into(),
                },
                ProjectionScope::Library,
            )
            .unwrap();
        assert_eq!(unchanged.disposition, ChangeDisposition::Unchanged);
        assert_eq!(unchanged.projection.excerpts[0].updated_at, 200);

        let removed = ExcerptThoughtLifecycle::new(&db, &FixedClock(400))
            .apply(
                ExcerptThoughtChange::RemoveThought { excerpt_id },
                ProjectionScope::Library,
            )
            .unwrap();
        assert_eq!(removed.disposition, ChangeDisposition::Changed);
        assert_eq!(removed.projection.counts.library_excerpts, 1);
        assert_eq!(removed.projection.counts.library_thoughts, 0);
        assert!(removed.projection.excerpts[0].thought.is_none());
    }

    #[test]
    fn deleting_excerpt_also_removes_thought_and_search_rows() {
        let db = test_db();
        let document = "before searchable quote after";
        let article_id = seed_article(&db, document);
        let created = ExcerptThoughtLifecycle::new(&db, &FixedClock(100))
            .apply(
                ExcerptThoughtChange::PutThought {
                    target: ExcerptTarget::Captured(capture(
                        article_id,
                        document,
                        "searchable quote",
                    )),
                    content: "searchable thought".into(),
                },
                ProjectionScope::Library,
            )
            .unwrap();
        let indexed: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM library_search_fts
                 WHERE source_kind IN ('excerpt','thought') AND source_id=?1",
                [created.affected_excerpt_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexed, 2);

        let deleted = ExcerptThoughtLifecycle::new(&db, &FixedClock(200))
            .apply(
                ExcerptThoughtChange::DeleteExcerpt {
                    excerpt_id: created.affected_excerpt_id,
                },
                ProjectionScope::Library,
            )
            .unwrap();
        assert_eq!(deleted.disposition, ChangeDisposition::Deleted);
        assert!(deleted.projection.excerpts.is_empty());
        let indexed: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM library_search_fts
                 WHERE source_kind IN ('excerpt','thought') AND source_id=?1",
                [created.affected_excerpt_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexed, 0);
    }

    #[test]
    fn resolution_becomes_unresolved_without_deleting_saved_material() {
        let db = test_db();
        let document = "before anchored quote after";
        let article_id = seed_article(&db, document);
        let created = ExcerptThoughtLifecycle::new(&db, &FixedClock(100))
            .apply(
                ExcerptThoughtChange::EnsureExcerpt {
                    capture: capture(article_id, document, "anchored quote"),
                },
                ProjectionScope::Article(article_id),
            )
            .unwrap();
        assert!(matches!(
            created.projection.excerpts[0].resolution,
            ExcerptResolution::Resolved { .. }
        ));
        db.conn
            .execute(
                "UPDATE articles SET content='the passage disappeared' WHERE id=?1",
                [article_id],
            )
            .unwrap();
        let projection = ExcerptThoughtLifecycle::new(&db, &FixedClock(200))
            .project(ProjectionScope::Article(article_id))
            .unwrap();
        assert_eq!(projection.excerpts.len(), 1);
        assert_eq!(
            projection.excerpts[0].resolution,
            ExcerptResolution::Unresolved
        );

        let repeated = ExcerptThoughtLifecycle::new(&db, &FixedClock(300))
            .apply(
                ExcerptThoughtChange::EnsureExcerpt {
                    capture: capture(article_id, document, "anchored quote"),
                },
                ProjectionScope::Article(article_id),
            )
            .unwrap();
        assert_eq!(repeated.disposition, ChangeDisposition::Unchanged);
        assert_eq!(repeated.affected_excerpt_id, created.affected_excerpt_id);
        assert_eq!(
            repeated.projection.excerpts[0].resolution,
            ExcerptResolution::Unresolved
        );
    }

    #[test]
    fn invalid_new_capture_anchor_is_rejected_without_creating_excerpt() {
        let db = test_db();
        let document = "before exact quote after";
        let article_id = seed_article(&db, document);
        let mut empty_range = capture(article_id, document, "exact quote");
        empty_range.anchor.end_offset = empty_range.anchor.start_offset;

        let structural = ExcerptThoughtLifecycle::new(&db, &FixedClock(100))
            .apply(
                ExcerptThoughtChange::EnsureExcerpt {
                    capture: empty_range,
                },
                ProjectionScope::Article(article_id),
            )
            .unwrap_err();
        assert_eq!(structural.kind, FailureKind::Input);

        let mut mismatched = capture(article_id, document, "exact quote");
        mismatched.anchor.start_offset = mismatched.anchor.start_offset.map(|value| value + 1);
        mismatched.anchor.end_offset = mismatched.anchor.end_offset.map(|value| value + 1);
        let semantic = ExcerptThoughtLifecycle::new(&db, &FixedClock(100))
            .apply(
                ExcerptThoughtChange::EnsureExcerpt {
                    capture: mismatched,
                },
                ProjectionScope::Article(article_id),
            )
            .unwrap_err();
        assert_eq!(semantic.kind, FailureKind::Input);
        assert!(
            semantic
                .technical_detail
                .contains("EXCERPT_ANCHOR_DOES_NOT_MATCH_CURRENT_ARTICLE")
        );
        let rows: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM article_selections", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn new_capture_uses_the_rendered_article_text_coordinate_space() {
        let db = test_db();
        let html = "<p>before <strong>exact quote</strong> after</p>";
        let article_id = seed_article(&db, html);
        let rendered = crate::article_document_presentation::article_selection_text(html, None);
        let outcome = ExcerptThoughtLifecycle::new(&db, &FixedClock(100))
            .apply(
                ExcerptThoughtChange::EnsureExcerpt {
                    capture: capture(article_id, &rendered, "exact quote"),
                },
                ProjectionScope::Article(article_id),
            )
            .unwrap();
        assert_eq!(outcome.disposition, ChangeDisposition::Created);
        assert!(matches!(
            outcome.projection.excerpts[0].resolution,
            ExcerptResolution::Resolved { .. }
        ));
    }

    #[test]
    fn missing_existing_target_is_not_reported_as_success() {
        let db = test_db();
        let article_id = seed_article(&db, "body");
        let failure = ExcerptThoughtLifecycle::new(&db, &FixedClock(100))
            .apply(
                ExcerptThoughtChange::DeleteExcerpt { excerpt_id: 999 },
                ProjectionScope::Article(article_id),
            )
            .unwrap_err();
        assert_eq!(failure.kind, FailureKind::NotFound);
    }

    #[test]
    fn v7_preserves_duplicate_legacy_thoughts_and_selects_one_representative() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE article_selections(
               id INTEGER PRIMARY KEY,
               article_id INTEGER NOT NULL,
               selected_text TEXT NOT NULL,
               start_offset INTEGER,
               end_offset INTEGER,
               anchor_prefix TEXT NOT NULL DEFAULT '',
               anchor_suffix TEXT NOT NULL DEFAULT '',
               comment TEXT,
               is_favorite INTEGER NOT NULL DEFAULT 0,
               created_at INTEGER NOT NULL,
               updated_at INTEGER NOT NULL
             );
             INSERT INTO article_selections
               (id,article_id,selected_text,start_offset,end_offset,anchor_prefix,anchor_suffix,comment,is_favorite,created_at,updated_at)
             VALUES
               (1,7,'quote',10,15,'before','after','older thought',0,1,10),
               (2,7,'quote',10,15,'before','after','newer thought',1,2,20),
               (3,7,'quote',NULL,NULL,'','','unanchored thought',0,3,30);",
        )
        .unwrap();
        migrate_to_v7(&conn).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM article_selections", [], |row| {
                row.get(0)
            })
            .unwrap();
        let managed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM article_selections WHERE lifecycle_identity IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let thoughts = {
            let mut statement = conn
                .prepare("SELECT comment FROM article_selections ORDER BY id")
                .unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(rows, 3);
        assert_eq!(managed, 1);
        assert_eq!(
            thoughts,
            vec!["older thought", "newer thought", "unanchored thought"]
        );
        let representative: i64 = conn
            .query_row(
                "SELECT id FROM article_selections WHERE lifecycle_identity IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(representative, 2);
    }

    #[test]
    fn opening_a_real_v6_file_adds_identity_before_creating_its_index() {
        let path = std::env::temp_dir().join(format!(
            "rrss-excerpt-v6-{}-{}.sqlite3",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE feeds(
                   id INTEGER PRIMARY KEY,url TEXT NOT NULL UNIQUE,title TEXT,interval_secs INTEGER,
                   last_fetch INTEGER,next_fetch INTEGER NOT NULL DEFAULT 0,last_error TEXT,
                   fail_count INTEGER NOT NULL DEFAULT 0,disabled INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE TABLE articles(
                   id INTEGER PRIMARY KEY,feed_id INTEGER NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
                   entry_id TEXT NOT NULL,url TEXT,title TEXT,author TEXT,published INTEGER,content TEXT,
                   is_read INTEGER NOT NULL DEFAULT 0,starred INTEGER NOT NULL DEFAULT 0,
                   read_later INTEGER NOT NULL DEFAULT 0,archived INTEGER NOT NULL DEFAULT 0,
                   fetched_at INTEGER NOT NULL,UNIQUE(feed_id,entry_id)
                 );
                 CREATE TABLE article_selections(
                   id INTEGER PRIMARY KEY,article_id INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
                   selected_text TEXT NOT NULL,start_offset INTEGER,end_offset INTEGER,
                   anchor_prefix TEXT NOT NULL DEFAULT '',anchor_suffix TEXT NOT NULL DEFAULT '',
                   comment TEXT,is_favorite INTEGER NOT NULL DEFAULT 0,
                   created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL
                 );
                 INSERT INTO feeds(id,url,next_fetch) VALUES(1,'https://example.test/feed',0);
                 INSERT INTO articles(id,feed_id,entry_id,title,content,fetched_at)
                   VALUES(1,1,'entry','Article','before quote after',1);
                 INSERT INTO article_selections
                   (id,article_id,selected_text,start_offset,end_offset,anchor_prefix,anchor_suffix,comment,is_favorite,created_at,updated_at)
                   VALUES(1,1,'quote',7,12,'before ',' after','legacy thought',0,1,1);",
            )
            .unwrap();
            crate::schema_evolution::evolve_fixture_to(&conn, 6).unwrap();
        }

        let db = Db::open(&path).unwrap();
        let version: i64 = db
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        let migrated: (bool, bool) = db
            .conn
            .query_row(
                "SELECT is_favorite,lifecycle_identity IS NOT NULL
                 FROM article_selections WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let index_exists: bool = db
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master
                 WHERE type='index' AND name='idx_article_selections_managed_identity')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let indexed_rows: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM library_search_fts
                 WHERE source_id=1 AND source_kind IN ('excerpt','thought')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, crate::schema_evolution::CURRENT_SCHEMA_VERSION);
        assert_eq!(migrated, (true, true));
        assert!(index_exists);
        assert_eq!(indexed_rows, 2);
        drop(db);
        for candidate in [
            path.clone(),
            std::path::PathBuf::from(format!("{}-wal", path.display())),
            std::path::PathBuf::from(format!("{}-shm", path.display())),
            std::path::PathBuf::from(format!("{}.maintenance.sqlite3", path.display())),
        ] {
            let _ = std::fs::remove_file(candidate);
        }
    }
}
