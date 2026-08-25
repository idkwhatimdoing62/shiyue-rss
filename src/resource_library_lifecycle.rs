//! Authoritative Resource Library lifecycle and projection seam.
//!
//! Callers submit complete human intent and adopt the SQLite projection
//! returned here. Curation, health, classification provenance, transactions,
//! maintenance fencing, and post-commit processing handoff stay behind this
//! module boundary.

#[allow(dead_code)]
mod store;

use std::collections::HashSet;

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};

use crate::db::{Db, WEB_CLIPPINGS_FEED_URL};
use crate::knowledge_workflow::resource_target;

pub(crate) use store::{
    Category, ImportCandidate, Pricing, Resource, ResourceCurationState, ResourceHealth,
    ResourceKind, ResourcePrivacy, ResourceSource, ResourceTag, SnapshotInput, TagLanguage,
    TagSource,
};

const DEFAULT_PAGE_SIZE: usize = 100;
const MAX_PAGE_SIZE: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResourceCollection {
    Active,
    PendingReview,
    Archived,
    Broken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResourceCursor {
    pub(crate) updated_at: i64,
    pub(crate) id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectionScope {
    Collection {
        collection: ResourceCollection,
        after: Option<ResourceCursor>,
        limit: usize,
    },
    Resource(i64),
}

impl ProjectionScope {
    pub(crate) fn collection(collection: ResourceCollection) -> Self {
        Self::Collection {
            collection,
            after: None,
            limit: DEFAULT_PAGE_SIZE,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ResourceLibraryCounts {
    pub(crate) active: usize,
    pub(crate) pending_review: usize,
    pub(crate) archived: usize,
    pub(crate) broken: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ResourceDetail {
    pub(crate) resource: Resource,
    pub(crate) categories: Vec<Category>,
    pub(crate) tags: Vec<ResourceTag>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct ResourceLibraryProjection {
    pub(crate) scope: ProjectionScope,
    pub(crate) resources: Vec<Resource>,
    pub(crate) detail: Option<ResourceDetail>,
    pub(crate) counts: ResourceLibraryCounts,
    pub(crate) next_cursor: Option<ResourceCursor>,
}

#[derive(Debug, Clone)]
pub(crate) struct CreateResource {
    pub(crate) url: String,
    pub(crate) parent_resource_id: Option<i64>,
    pub(crate) linked_article_id: Option<i64>,
    pub(crate) kind: ResourceKind,
    pub(crate) title: Option<String>,
    pub(crate) private_note: Option<String>,
    pub(crate) privacy: ResourcePrivacy,
    pub(crate) source: ResourceSource,
    pub(crate) manual_rating: Option<i64>,
}

#[derive(Debug, Clone)]
pub(crate) struct CompleteManualEdit {
    pub(crate) resource_id: i64,
    pub(crate) title: Option<String>,
    pub(crate) purpose_zh: Option<String>,
    pub(crate) use_when_zh: Option<String>,
    pub(crate) private_note: Option<String>,
    pub(crate) privacy: ResourcePrivacy,
    pub(crate) manual_rating: Option<i64>,
    pub(crate) categories: Vec<Category>,
    pub(crate) tags: Vec<ResourceTag>,
}

#[derive(Debug, Clone)]
pub(crate) enum ResourceLifecycleChange {
    Create(CreateResource),
    CompleteManualEdit(CompleteManualEdit),
    SetCurationState {
        resource_id: i64,
        target: ResourceCurationState,
    },
    Delete {
        resource_id: i64,
    },
    ImportWebClippings {
        article_ids: Vec<i64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeDisposition {
    Created,
    Existing,
    Changed,
    Unchanged,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HandoffDisposition {
    Queued,
    Deferred {
        user_message: String,
        technical_detail: String,
    },
    NotRequested,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResourceHandoff {
    pub(crate) resource_id: i64,
    pub(crate) disposition: HandoffDisposition,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct ApplyOutcome {
    pub(crate) disposition: ChangeDisposition,
    pub(crate) affected_resource_ids: Vec<i64>,
    pub(crate) handoffs: Vec<ResourceHandoff>,
    pub(crate) projection: ResourceLibraryProjection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureKind {
    Input,
    NotFound,
    InvalidTransition,
    ProcessingActive,
    Maintenance,
    Storage,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{user_message}")]
pub(crate) struct LifecycleFailure {
    pub(crate) kind: FailureKind,
    pub(crate) user_message: String,
    pub(crate) technical_detail: String,
}

impl LifecycleFailure {
    fn input(message: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Input,
            user_message: message.into(),
            technical_detail: detail.into(),
        }
    }

    fn not_found(id: i64) -> Self {
        Self {
            kind: FailureKind::NotFound,
            user_message: "资源不存在或已经被删除".into(),
            technical_detail: format!("RESOURCE_NOT_FOUND: {id}"),
        }
    }

    fn processing_active(id: i64) -> Self {
        Self {
            kind: FailureKind::ProcessingActive,
            user_message: "资源正在整理，请等待完成后重试".into(),
            technical_detail: format!("RESOURCE_PROCESSING_ACTIVE: {id}"),
        }
    }

    fn storage(error: impl std::fmt::Display) -> Self {
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
                "资料维护期间不能修改资源".into()
            } else {
                "资源资料操作失败".into()
            },
            technical_detail: detail,
        }
    }

    fn projection(error: anyhow::Error) -> Self {
        match error.downcast::<Self>() {
            Ok(failure) => failure,
            Err(error) => Self::storage(error),
        }
    }
}

pub(crate) trait ProcessingHandoff {
    fn request_resource_processing(&self, resource_id: i64) -> Result<(), String>;
}

pub(crate) trait Clock {
    fn now(&self) -> i64;
}

#[derive(Debug, Default)]
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        Utc::now().timestamp()
    }
}

#[derive(Debug, Default)]
pub(crate) struct NoProcessingHandoff;

impl ProcessingHandoff for NoProcessingHandoff {
    fn request_resource_processing(&self, _resource_id: i64) -> Result<(), String> {
        Err("KNOWLEDGE_PROCESSING_NOT_CONNECTED".into())
    }
}

pub(crate) struct ResourceLibraryLifecycle<'db, 'adapter> {
    db: &'db Db,
    handoff: &'adapter dyn ProcessingHandoff,
    clock: &'adapter dyn Clock,
}

impl<'db, 'adapter> ResourceLibraryLifecycle<'db, 'adapter> {
    pub(crate) fn new(
        db: &'db Db,
        handoff: &'adapter dyn ProcessingHandoff,
        clock: &'adapter dyn Clock,
    ) -> Self {
        Self { db, handoff, clock }
    }

    pub(crate) fn project(
        &self,
        scope: ProjectionScope,
    ) -> Result<ResourceLibraryProjection, LifecycleFailure> {
        build_projection(&self.db.conn, scope).map_err(LifecycleFailure::projection)
    }

    pub(crate) fn preview_web_clipping_import(
        &self,
    ) -> Result<Vec<ImportCandidate>, LifecycleFailure> {
        let mut candidates = Vec::new();
        for article in self.db.web_clippings().map_err(LifecycleFailure::storage)? {
            let Some(url) = article.url else { continue };
            let canonical = store::canonicalize_url(&url).map_err(LifecycleFailure::storage)?;
            let existing = self
                .db
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM resources WHERE canonical_url=?1 OR (source='import' AND linked_article_id=?2)",
                    params![canonical, article.id],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(LifecycleFailure::storage)?;
            candidates.push(ImportCandidate {
                article_id: article.id,
                url,
                title: article.title,
                already_imported: existing > 0,
            });
        }
        Ok(candidates)
    }

    pub(crate) fn apply(
        &self,
        change: ResourceLifecycleChange,
        refresh_scope: ProjectionScope,
    ) -> Result<ApplyOutcome, LifecycleFailure> {
        validate_change(&change)?;
        let permit = self.db.write_permit().map_err(LifecycleFailure::storage)?;
        let now = self.clock.now();
        let tx = self
            .db
            .conn
            .unchecked_transaction()
            .map_err(LifecycleFailure::storage)?;
        let mut processing_ids = Vec::new();
        let (disposition, affected_resource_ids) =
            apply_change(&tx, change, now, &mut processing_ids)?;
        let projection =
            build_projection(&tx, refresh_scope).map_err(LifecycleFailure::projection)?;
        if let Some(permit) = permit.as_ref() {
            permit.validate().map_err(LifecycleFailure::storage)?;
        }
        tx.commit().map_err(LifecycleFailure::storage)?;

        let requested = processing_ids.iter().copied().collect::<HashSet<_>>();
        let handoffs = affected_resource_ids
            .iter()
            .copied()
            .map(|resource_id| ResourceHandoff {
                resource_id,
                disposition: if requested.contains(&resource_id) {
                    match self.handoff.request_resource_processing(resource_id) {
                        Ok(()) => HandoffDisposition::Queued,
                        Err(technical_detail) => HandoffDisposition::Deferred {
                            user_message: "资源已保存，后台整理暂未启动，可稍后重试".into(),
                            technical_detail,
                        },
                    }
                } else {
                    HandoffDisposition::NotRequested
                },
            })
            .collect();
        Ok(ApplyOutcome {
            disposition,
            affected_resource_ids,
            handoffs,
            projection,
        })
    }
}

fn validate_change(change: &ResourceLifecycleChange) -> Result<(), LifecycleFailure> {
    match change {
        ResourceLifecycleChange::Create(input) => validate_rating(input.manual_rating),
        ResourceLifecycleChange::CompleteManualEdit(edit) => {
            validate_id(edit.resource_id)?;
            validate_rating(edit.manual_rating)?;
            let mut names = HashSet::new();
            for tag in &edit.tags {
                let name = tag.name.trim();
                if name.is_empty() || name.chars().count() > 100 {
                    return Err(LifecycleFailure::input("标签无效", "INVALID_RESOURCE_TAG"));
                }
                if !names.insert(name.to_lowercase()) {
                    return Err(LifecycleFailure::input(
                        "标签不能重复",
                        "DUPLICATE_RESOURCE_TAG",
                    ));
                }
            }
            Ok(())
        }
        ResourceLifecycleChange::SetCurationState { resource_id, .. }
        | ResourceLifecycleChange::Delete { resource_id } => validate_id(*resource_id),
        ResourceLifecycleChange::ImportWebClippings { article_ids } => {
            if article_ids.is_empty() {
                return Err(LifecycleFailure::input(
                    "请选择要导入的网页收藏",
                    "EMPTY_WEB_CLIPPING_IMPORT",
                ));
            }
            let unique = article_ids.iter().copied().collect::<HashSet<_>>();
            if unique.len() != article_ids.len() || article_ids.iter().any(|id| *id <= 0) {
                return Err(LifecycleFailure::input(
                    "网页收藏选择无效",
                    "INVALID_WEB_CLIPPING_IMPORT",
                ));
            }
            Ok(())
        }
    }
}

fn validate_id(id: i64) -> Result<(), LifecycleFailure> {
    if id <= 0 {
        Err(LifecycleFailure::input(
            "资源标识无效",
            format!("INVALID_RESOURCE_ID: {id}"),
        ))
    } else {
        Ok(())
    }
}

fn validate_rating(rating: Option<i64>) -> Result<(), LifecycleFailure> {
    if rating.is_some_and(|rating| !(1..=5).contains(&rating)) {
        Err(LifecycleFailure::input(
            "评分必须是 1 到 5",
            "INVALID_RESOURCE_RATING",
        ))
    } else {
        Ok(())
    }
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_owned();
        (!value.is_empty()).then_some(value)
    })
}

fn apply_change(
    conn: &Connection,
    change: ResourceLifecycleChange,
    now: i64,
    processing_ids: &mut Vec<i64>,
) -> Result<(ChangeDisposition, Vec<i64>), LifecycleFailure> {
    match change {
        ResourceLifecycleChange::Create(input) => {
            let canonical =
                store::canonicalize_url(&input.url).map_err(LifecycleFailure::storage)?;
            if let Some(id) = conn
                .query_row(
                    "SELECT id FROM resources WHERE canonical_url=?1",
                    [&canonical],
                    |row| row.get(0),
                )
                .optional()
                .map_err(LifecycleFailure::storage)?
            {
                return Ok((ChangeDisposition::Existing, vec![id]));
            }
            let curation = if input.source == ResourceSource::CliAgent {
                ResourceCurationState::PendingReview
            } else {
                ResourceCurationState::Active
            };
            let legacy_status = if curation == ResourceCurationState::PendingReview {
                "pending_review"
            } else {
                "active"
            };
            conn.execute(
                "INSERT INTO resources(
                   url,canonical_url,parent_resource_id,linked_article_id,kind,title,private_note,
                   privacy,status,curation_state,health,categories_source,tags_source,source,
                   manual_rating,created_at,updated_at
                 ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'unknown','ai','ai',?11,?12,?13,?13)",
                params![
                    input.url.trim(),
                    canonical,
                    input.parent_resource_id,
                    input.linked_article_id,
                    input.kind.as_str(),
                    normalize_optional(input.title),
                    normalize_optional(input.private_note),
                    input.privacy.as_str(),
                    legacy_status,
                    curation.as_str(),
                    input.source.as_str(),
                    input.manual_rating,
                    now
                ],
            )
            .map_err(LifecycleFailure::storage)?;
            let id = conn.last_insert_rowid();
            store::refresh_search_index_on(conn, id).map_err(LifecycleFailure::storage)?;
            if input.source != ResourceSource::CliAgent {
                processing_ids.push(id);
            }
            Ok((ChangeDisposition::Created, vec![id]))
        }
        ResourceLifecycleChange::CompleteManualEdit(edit) => {
            let current = get_resource(conn, edit.resource_id)?;
            if current.privacy == ResourcePrivacy::Public
                && edit.privacy == ResourcePrivacy::Private
                && resource_target::has_active_processing(conn, edit.resource_id)
                    .map_err(LifecycleFailure::storage)?
            {
                return Err(LifecycleFailure::processing_active(edit.resource_id));
            }
            let title = normalize_optional(edit.title);
            let purpose = normalize_optional(edit.purpose_zh);
            let use_when = normalize_optional(edit.use_when_zh);
            let private_note = normalize_optional(edit.private_note);
            conn.execute(
                "UPDATE resources SET title=?2,purpose_zh=?3,purpose_source='manual',
                   use_when_zh=?4,use_when_source='manual',private_note=?5,privacy=?6,
                   manual_rating=?7,categories_source='manual',tags_source='manual',updated_at=?8
                 WHERE id=?1",
                params![
                    edit.resource_id,
                    title,
                    purpose,
                    use_when,
                    private_note,
                    edit.privacy.as_str(),
                    edit.manual_rating,
                    now
                ],
            )
            .map_err(LifecycleFailure::storage)?;
            conn.execute(
                "DELETE FROM resource_categories WHERE resource_id=?1",
                [edit.resource_id],
            )
            .map_err(LifecycleFailure::storage)?;
            for category in edit.categories {
                conn.execute(
                    "INSERT INTO resource_categories(resource_id,category) VALUES(?1,?2)",
                    params![edit.resource_id, category.as_str()],
                )
                .map_err(LifecycleFailure::storage)?;
            }
            conn.execute(
                "DELETE FROM resource_tags WHERE resource_id=?1",
                [edit.resource_id],
            )
            .map_err(LifecycleFailure::storage)?;
            for tag in edit.tags {
                conn.execute(
                    "INSERT INTO resource_tags(resource_id,name,language,source,created_at)
                     VALUES(?1,?2,?3,'manual',?4)",
                    params![
                        edit.resource_id,
                        tag.name.trim(),
                        tag.language.as_str(),
                        now
                    ],
                )
                .map_err(LifecycleFailure::storage)?;
            }
            store::refresh_search_index_on(conn, edit.resource_id)
                .map_err(LifecycleFailure::storage)?;
            Ok((ChangeDisposition::Changed, vec![edit.resource_id]))
        }
        ResourceLifecycleChange::SetCurationState {
            resource_id,
            target,
        } => {
            let current = get_resource(conn, resource_id)?;
            if current.curation_state == target {
                return Ok((ChangeDisposition::Unchanged, vec![resource_id]));
            }
            let valid = matches!(
                (current.curation_state, target),
                (
                    ResourceCurationState::PendingReview,
                    ResourceCurationState::Active
                ) | (
                    ResourceCurationState::PendingReview,
                    ResourceCurationState::Archived
                ) | (
                    ResourceCurationState::Active,
                    ResourceCurationState::Archived
                ) | (
                    ResourceCurationState::Archived,
                    ResourceCurationState::Active
                )
            );
            if !valid {
                return Err(LifecycleFailure {
                    kind: FailureKind::InvalidTransition,
                    user_message: "资源状态不能这样变更".into(),
                    technical_detail: format!(
                        "INVALID_RESOURCE_TRANSITION: {:?} -> {:?}",
                        current.curation_state, target
                    ),
                });
            }
            let legacy_status = match target {
                ResourceCurationState::PendingReview => "pending_review",
                ResourceCurationState::Active => "active",
                ResourceCurationState::Archived => "archived",
            };
            conn.execute(
                "UPDATE resources SET curation_state=?2,status=?3,updated_at=?4 WHERE id=?1",
                params![resource_id, target.as_str(), legacy_status, now],
            )
            .map_err(LifecycleFailure::storage)?;
            if current.curation_state == ResourceCurationState::PendingReview
                && target == ResourceCurationState::Active
            {
                processing_ids.push(resource_id);
            }
            Ok((ChangeDisposition::Changed, vec![resource_id]))
        }
        ResourceLifecycleChange::Delete { resource_id } => {
            let current = get_resource(conn, resource_id)?;
            if !matches!(
                current.curation_state,
                ResourceCurationState::PendingReview | ResourceCurationState::Archived
            ) {
                return Err(LifecycleFailure {
                    kind: FailureKind::InvalidTransition,
                    user_message: "请先归档资源，再永久删除".into(),
                    technical_detail: format!("RESOURCE_DELETE_REQUIRES_ARCHIVE: {resource_id}"),
                });
            }
            resource_target::prepare_delete(conn, resource_id).map_err(|error| {
                if error.to_string().contains("RESOURCE_PROCESSING_ACTIVE") {
                    LifecycleFailure::processing_active(resource_id)
                } else {
                    LifecycleFailure::storage(error)
                }
            })?;
            conn.execute("DELETE FROM resource_fts WHERE source_id=?1", [resource_id])
                .map_err(LifecycleFailure::storage)?;
            conn.execute("DELETE FROM resources WHERE id=?1", [resource_id])
                .map_err(LifecycleFailure::storage)?;
            Ok((ChangeDisposition::Deleted, vec![resource_id]))
        }
        ResourceLifecycleChange::ImportWebClippings { article_ids } => {
            let mut ids = Vec::new();
            let mut created = false;
            for article_id in article_ids {
                let row = conn
                    .query_row(
                        "SELECT a.url,a.title FROM articles a JOIN feeds f ON f.id=a.feed_id
                         WHERE a.id=?1 AND f.url=?2 AND a.url IS NOT NULL",
                        params![article_id, WEB_CLIPPINGS_FEED_URL],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
                    )
                    .optional()
                    .map_err(LifecycleFailure::storage)?
                    .ok_or_else(|| {
                        LifecycleFailure::input(
                            "所选内容不是有效的网页收藏",
                            format!("INVALID_WEB_CLIPPING: {article_id}"),
                        )
                    })?;
                let canonical =
                    store::canonicalize_url(&row.0).map_err(LifecycleFailure::storage)?;
                if let Some(id) = conn
                    .query_row(
                        "SELECT id FROM resources WHERE canonical_url=?1 OR
                         (source='import' AND linked_article_id=?2)",
                        params![canonical, article_id],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(LifecycleFailure::storage)?
                {
                    ids.push(id);
                    continue;
                }
                conn.execute(
                    "INSERT INTO resources(
                       url,canonical_url,linked_article_id,kind,title,privacy,status,curation_state,
                       health,categories_source,tags_source,source,created_at,updated_at
                     ) VALUES(?1,?2,?3,'article',?4,'public','active','active','healthy','ai','ai','import',?5,?5)",
                    params![row.0, canonical, article_id, row.1, now],
                )
                .map_err(LifecycleFailure::storage)?;
                let id = conn.last_insert_rowid();
                store::refresh_search_index_on(conn, id).map_err(LifecycleFailure::storage)?;
                ids.push(id);
                processing_ids.push(id);
                created = true;
            }
            Ok((
                if created {
                    ChangeDisposition::Created
                } else {
                    ChangeDisposition::Existing
                },
                ids,
            ))
        }
    }
}

fn get_resource(conn: &Connection, id: i64) -> Result<Resource, LifecycleFailure> {
    conn.query_row(
        &format!("SELECT {} FROM resources WHERE id=?1", store::RESOURCE_COLS),
        [id],
        store::map_resource,
    )
    .optional()
    .map_err(LifecycleFailure::storage)?
    .ok_or_else(|| LifecycleFailure::not_found(id))
}

fn build_projection(
    conn: &Connection,
    scope: ProjectionScope,
) -> Result<ResourceLibraryProjection, anyhow::Error> {
    let counts = conn.query_row(
        "SELECT
           SUM(curation_state='active'),
           SUM(curation_state='pending_review'),
           SUM(curation_state='archived'),
           SUM(curation_state<>'archived' AND health='broken')
         FROM resources",
        [],
        |row| {
            Ok(ResourceLibraryCounts {
                active: row.get::<_, Option<i64>>(0)?.unwrap_or(0) as usize,
                pending_review: row.get::<_, Option<i64>>(1)?.unwrap_or(0) as usize,
                archived: row.get::<_, Option<i64>>(2)?.unwrap_or(0) as usize,
                broken: row.get::<_, Option<i64>>(3)?.unwrap_or(0) as usize,
            })
        },
    )?;
    match scope {
        ProjectionScope::Resource(id) => {
            let resource = get_resource(conn, id).map_err(anyhow::Error::new)?;
            let detail = load_detail(conn, resource.clone())?;
            Ok(ResourceLibraryProjection {
                scope,
                resources: vec![resource],
                detail: Some(detail),
                counts,
                next_cursor: None,
            })
        }
        ProjectionScope::Collection {
            collection,
            after,
            limit,
        } => {
            let limit = limit.clamp(1, MAX_PAGE_SIZE);
            let predicate = match collection {
                ResourceCollection::Active => "curation_state='active'",
                ResourceCollection::PendingReview => "curation_state='pending_review'",
                ResourceCollection::Archived => "curation_state='archived'",
                ResourceCollection::Broken => "curation_state<>'archived' AND health='broken'",
            };
            let sql = format!(
                "SELECT {} FROM resources WHERE {predicate}
                 AND (?1 IS NULL OR updated_at < ?1 OR (updated_at=?1 AND id < ?2))
                 ORDER BY updated_at DESC,id DESC LIMIT ?3",
                store::RESOURCE_COLS
            );
            let mut statement = conn.prepare(&sql)?;
            let rows = statement.query_map(
                params![
                    after.map(|cursor| cursor.updated_at),
                    after.map(|cursor| cursor.id),
                    i64::try_from(limit + 1).unwrap_or(i64::MAX)
                ],
                store::map_resource,
            )?;
            let mut resources = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            let has_more = resources.len() > limit;
            resources.truncate(limit);
            let next_cursor = has_more.then(|| {
                let last = resources.last().expect("page with overflow is non-empty");
                ResourceCursor {
                    updated_at: last.updated_at,
                    id: last.id,
                }
            });
            Ok(ResourceLibraryProjection {
                scope,
                resources,
                detail: None,
                counts,
                next_cursor,
            })
        }
    }
}

fn load_detail(conn: &Connection, resource: Resource) -> anyhow::Result<ResourceDetail> {
    let mut category_statement = conn.prepare(
        "SELECT category FROM resource_categories WHERE resource_id=?1 ORDER BY category",
    )?;
    let category_values = category_statement
        .query_map([resource.id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let categories = category_values
        .into_iter()
        .map(|value| Category::parse(&value))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut tag_statement = conn.prepare(
        "SELECT name,language,source FROM resource_tags WHERE resource_id=?1
         ORDER BY name COLLATE NOCASE",
    )?;
    let raw_tags = tag_statement
        .query_map([resource.id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let tags = raw_tags
        .into_iter()
        .map(|(name, language, source)| {
            Ok(ResourceTag {
                name,
                language: TagLanguage::parse(&language)?,
                source: TagSource::parse(&source)?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(ResourceDetail {
        resource,
        categories,
        tags,
    })
}

pub(crate) fn legacy_search_json(
    db: &Db,
    query: &str,
    include_resources: bool,
    include_articles: bool,
    all_articles: bool,
    limit: usize,
) -> anyhow::Result<Vec<serde_json::Value>> {
    store::ResourceStore::new(db).search_json(
        query,
        include_resources,
        include_articles,
        all_articles,
        limit,
    )
}

pub(crate) fn resource_json(
    db: &Db,
    resource: &Resource,
    matched_field: &str,
    snippet: String,
    score: f64,
) -> anyhow::Result<serde_json::Value> {
    store::ResourceStore::new(db).to_json(resource, matched_field, snippet, score)
}

pub(crate) fn load_processing_resource(db: &Db, id: i64) -> anyhow::Result<Resource> {
    get_resource(&db.conn, id).map_err(anyhow::Error::new)
}

pub(crate) fn build_processing_input(
    db: &Db,
    id: i64,
) -> anyhow::Result<Option<crate::resource_enrichment::EnrichmentInput>> {
    store::ResourceStore::new(db).enrichment_input(id)
}

pub(crate) fn refresh_processing_search_index(conn: &Connection, id: i64) -> anyhow::Result<()> {
    store::refresh_search_index_on(conn, id)
}

pub(crate) fn apply_processing_enrichment(
    conn: &Connection,
    id: i64,
    output: &crate::resource_enrichment::EnrichmentOutput,
    now: i64,
) -> anyhow::Result<()> {
    store::ResourceStore::apply_enrichment_on(conn, id, output, now)
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[derive(Debug)]
    struct FixedClock(i64);
    impl Clock for FixedClock {
        fn now(&self) -> i64 {
            self.0
        }
    }

    #[derive(Debug, Default)]
    struct RecordingHandoff(std::sync::Mutex<Vec<i64>>);
    impl ProcessingHandoff for RecordingHandoff {
        fn request_resource_processing(&self, resource_id: i64) -> Result<(), String> {
            self.0.lock().unwrap().push(resource_id);
            Ok(())
        }
    }

    fn lifecycle<'a>(
        db: &'a Db,
        handoff: &'a RecordingHandoff,
        clock: &'a FixedClock,
    ) -> ResourceLibraryLifecycle<'a, 'a> {
        ResourceLibraryLifecycle::new(db, handoff, clock)
    }

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        Db {
            conn,
            path: None,
            _writer_gate: None,
            _lifetime_permit: None,
        }
    }

    #[test]
    fn cli_creation_waits_for_review_and_is_idempotent() {
        let db = test_db();
        let handoff = RecordingHandoff::default();
        let clock = FixedClock(100);
        let create = CreateResource {
            url: "https://example.com/tool".into(),
            parent_resource_id: None,
            linked_article_id: None,
            kind: ResourceKind::Page,
            title: Some(" Tool ".into()),
            private_note: None,
            privacy: ResourcePrivacy::Public,
            source: ResourceSource::CliAgent,
            manual_rating: None,
        };
        let first = lifecycle(&db, &handoff, &clock)
            .apply(
                ResourceLifecycleChange::Create(create.clone()),
                ProjectionScope::collection(ResourceCollection::PendingReview),
            )
            .unwrap();
        assert_eq!(first.disposition, ChangeDisposition::Created);
        assert_eq!(first.projection.counts.pending_review, 1);
        assert!(handoff.0.lock().unwrap().is_empty());
        let second = lifecycle(&db, &handoff, &clock)
            .apply(
                ResourceLifecycleChange::Create(create),
                ProjectionScope::collection(ResourceCollection::PendingReview),
            )
            .unwrap();
        assert_eq!(second.disposition, ChangeDisposition::Existing);
        assert_eq!(second.affected_resource_ids, first.affected_resource_ids);
    }

    #[test]
    fn activating_reviewed_resource_requests_processing_after_commit() {
        let db = test_db();
        let handoff = RecordingHandoff::default();
        let clock = FixedClock(100);
        let created = lifecycle(&db, &handoff, &clock)
            .apply(
                ResourceLifecycleChange::Create(CreateResource {
                    url: "https://example.com/docs".into(),
                    parent_resource_id: None,
                    linked_article_id: None,
                    kind: ResourceKind::Page,
                    title: None,
                    private_note: None,
                    privacy: ResourcePrivacy::Public,
                    source: ResourceSource::CliAgent,
                    manual_rating: None,
                }),
                ProjectionScope::collection(ResourceCollection::PendingReview),
            )
            .unwrap();
        let id = created.affected_resource_ids[0];
        let outcome = lifecycle(&db, &handoff, &clock)
            .apply(
                ResourceLifecycleChange::SetCurationState {
                    resource_id: id,
                    target: ResourceCurationState::Active,
                },
                ProjectionScope::collection(ResourceCollection::Active),
            )
            .unwrap();
        assert_eq!(outcome.disposition, ChangeDisposition::Changed);
        assert_eq!(*handoff.0.lock().unwrap(), vec![id]);
        assert_eq!(
            outcome.projection.resources[0].curation_state,
            ResourceCurationState::Active
        );
    }

    #[test]
    fn complete_manual_edit_replaces_all_editable_fields_atomically() {
        let db = test_db();
        let handoff = RecordingHandoff::default();
        let clock = FixedClock(100);
        let created = lifecycle(&db, &handoff, &clock)
            .apply(
                ResourceLifecycleChange::Create(CreateResource {
                    url: "https://example.com/reference".into(),
                    parent_resource_id: None,
                    linked_article_id: None,
                    kind: ResourceKind::Site,
                    title: None,
                    private_note: None,
                    privacy: ResourcePrivacy::Public,
                    source: ResourceSource::Gui,
                    manual_rating: None,
                }),
                ProjectionScope::collection(ResourceCollection::Active),
            )
            .unwrap();
        let id = created.affected_resource_ids[0];

        let outcome = lifecycle(&db, &handoff, &FixedClock(200))
            .apply(
                ResourceLifecycleChange::CompleteManualEdit(CompleteManualEdit {
                    resource_id: id,
                    title: Some(" Reference ".into()),
                    purpose_zh: Some("查阅接口".into()),
                    use_when_zh: Some("实现客户端时".into()),
                    private_note: Some("团队内部".into()),
                    privacy: ResourcePrivacy::Private,
                    manual_rating: Some(5),
                    categories: vec![Category::Docs],
                    tags: vec![ResourceTag {
                        name: "API".into(),
                        language: TagLanguage::En,
                        source: TagSource::Ai,
                    }],
                }),
                ProjectionScope::Resource(id),
            )
            .unwrap();
        let detail = outcome.projection.detail.unwrap();
        assert_eq!(detail.resource.title.as_deref(), Some("Reference"));
        assert_eq!(detail.resource.purpose_zh.as_deref(), Some("查阅接口"));
        assert_eq!(detail.resource.use_when_zh.as_deref(), Some("实现客户端时"));
        assert_eq!(detail.resource.private_note.as_deref(), Some("团队内部"));
        assert_eq!(detail.resource.privacy, ResourcePrivacy::Private);
        assert_eq!(detail.resource.manual_rating, Some(5));
        assert_eq!(detail.categories, vec![Category::Docs]);
        assert_eq!(detail.tags[0].source, TagSource::Manual);
        let provenance: (String, String, String, String) = db
            .conn
            .query_row(
                "SELECT purpose_source,use_when_source,categories_source,tags_source
                 FROM resources WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            provenance,
            (
                "manual".into(),
                "manual".into(),
                "manual".into(),
                "manual".into()
            )
        );
    }

    #[test]
    fn delete_rejects_active_processing_then_purges_terminal_history() {
        let db = test_db();
        let handoff = RecordingHandoff::default();
        let clock = FixedClock(100);
        let created = lifecycle(&db, &handoff, &clock)
            .apply(
                ResourceLifecycleChange::Create(CreateResource {
                    url: "https://example.com/delete-me".into(),
                    parent_resource_id: None,
                    linked_article_id: None,
                    kind: ResourceKind::Page,
                    title: None,
                    private_note: None,
                    privacy: ResourcePrivacy::Public,
                    source: ResourceSource::CliAgent,
                    manual_rating: None,
                }),
                ProjectionScope::collection(ResourceCollection::PendingReview),
            )
            .unwrap();
        let id = created.affected_resource_ids[0];
        db.conn
            .execute(
                "INSERT INTO knowledge_tasks(
                   kind,target_id,status,next_run_at,created_at,updated_at
                 ) VALUES('resource_completion',?1,'queued',0,100,100)",
                [id],
            )
            .unwrap();

        let failure = lifecycle(&db, &handoff, &clock)
            .apply(
                ResourceLifecycleChange::Delete { resource_id: id },
                ProjectionScope::collection(ResourceCollection::PendingReview),
            )
            .unwrap_err();
        assert_eq!(failure.kind, FailureKind::ProcessingActive);
        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM resources WHERE id=?1", [id], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            1
        );

        db.conn
            .execute(
                "UPDATE knowledge_tasks SET status='succeeded' WHERE target_id=?1",
                [id],
            )
            .unwrap();
        let deleted = lifecycle(&db, &handoff, &clock)
            .apply(
                ResourceLifecycleChange::Delete { resource_id: id },
                ProjectionScope::collection(ResourceCollection::PendingReview),
            )
            .unwrap();
        assert_eq!(deleted.disposition, ChangeDisposition::Deleted);
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM knowledge_tasks WHERE target_id=?1",
                    [id],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn web_clipping_import_rolls_back_when_any_selection_is_invalid() {
        let db = test_db();
        let handoff = RecordingHandoff::default();
        let clock = FixedClock(100);
        let article_id = db
            .save_web_clipping(
                Some("https://example.com/saved"),
                Some("Saved"),
                "<main>saved</main>",
                50,
            )
            .unwrap();

        let failure = lifecycle(&db, &handoff, &clock)
            .apply(
                ResourceLifecycleChange::ImportWebClippings {
                    article_ids: vec![article_id, 999_999],
                },
                ProjectionScope::collection(ResourceCollection::Active),
            )
            .unwrap_err();
        assert_eq!(failure.kind, FailureKind::Input);
        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM resources", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(handoff.0.lock().unwrap().is_empty());
    }

    #[test]
    fn broken_projection_overlaps_active_and_uses_a_stable_cursor() {
        let db = test_db();
        let handoff = RecordingHandoff::default();
        for (now, path) in [(100, "one"), (200, "two"), (300, "three")] {
            lifecycle(&db, &handoff, &FixedClock(now))
                .apply(
                    ResourceLifecycleChange::Create(CreateResource {
                        url: format!("https://example.com/{path}"),
                        parent_resource_id: None,
                        linked_article_id: None,
                        kind: ResourceKind::Page,
                        title: Some(path.into()),
                        private_note: None,
                        privacy: ResourcePrivacy::Public,
                        source: ResourceSource::Gui,
                        manual_rating: None,
                    }),
                    ProjectionScope::collection(ResourceCollection::Active),
                )
                .unwrap();
        }
        let broken_id: i64 = db
            .conn
            .query_row("SELECT id FROM resources WHERE title='two'", [], |row| {
                row.get(0)
            })
            .unwrap();
        db.conn
            .execute(
                "UPDATE resources SET health='broken' WHERE id=?1",
                [broken_id],
            )
            .unwrap();

        let broken = lifecycle(&db, &handoff, &FixedClock(400))
            .project(ProjectionScope::collection(ResourceCollection::Broken))
            .unwrap();
        assert_eq!(broken.resources[0].id, broken_id);
        assert_eq!(broken.counts.active, 3);
        assert_eq!(broken.counts.broken, 1);

        let first = lifecycle(&db, &handoff, &FixedClock(400))
            .project(ProjectionScope::Collection {
                collection: ResourceCollection::Active,
                after: None,
                limit: 1,
            })
            .unwrap();
        let second = lifecycle(&db, &handoff, &FixedClock(400))
            .project(ProjectionScope::Collection {
                collection: ResourceCollection::Active,
                after: first.next_cursor,
                limit: 1,
            })
            .unwrap();
        assert_ne!(first.resources[0].id, second.resources[0].id);
    }
}
