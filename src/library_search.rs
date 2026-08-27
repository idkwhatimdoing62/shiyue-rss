//! Unified Library Search and ranking seam.
//!
//! GUI, CLI, and agent adapters submit the same request and receive the same
//! typed, relevance-ordered results. SQLite FTS shape, eligibility, grouping,
//! scoring, evidence, history, and time budgeting remain implementation details.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::time::{Duration, Instant};

use rusqlite::{Connection, params};
use unicode_normalization::UnicodeNormalization;

use crate::db::{Db, WEB_CLIPPINGS_FEED_URL};
use crate::model::SearchHistoryEntry;
use crate::resource_library_lifecycle::canonicalize_url;

const DEFAULT_BUDGET: Duration = Duration::from_secs(2);
const MAX_RESULTS: usize = 200;
const MAX_EVIDENCE: usize = 6;

const SEARCH_INDEX_SCHEMA: &str = r#"
CREATE VIRTUAL TABLE library_search_fts USING fts5(
  source_kind UNINDEXED,
  source_id UNINDEXED,
  article_id UNINDEXED,
  canonical_url UNINDEXED,
  updated_at UNINDEXED,
  identity_text,
  title_text,
  metadata_text,
  note_text,
  excerpt_text,
  body_text,
  tokenize='trigram'
);

CREATE TRIGGER resources_search_insert AFTER INSERT ON resources BEGIN
  INSERT INTO library_search_fts(
    source_kind,source_id,article_id,canonical_url,updated_at,
    identity_text,title_text,metadata_text,note_text,excerpt_text,body_text
  )
  SELECT 'resource',r.id,r.linked_article_id,r.canonical_url,r.updated_at,
         r.url||char(10)||r.canonical_url,
         COALESCE(r.title,''),
         trim(COALESCE(r.purpose_zh,'')||char(10)||COALESCE(r.use_when_zh,'')||char(10)||
              r.capabilities||char(10)||r.limitations||char(10)||r.languages||char(10)||
              COALESCE((SELECT group_concat(category,' ') FROM resource_categories c WHERE c.resource_id=r.id),'')||char(10)||
              COALESCE((SELECT group_concat(name,' ') FROM resource_tags t WHERE t.resource_id=r.id),'')),
         COALESCE(r.private_note,''),'',
         COALESCE((SELECT cleaned_content FROM resource_snapshots s WHERE s.id=r.latest_snapshot_id),'')
  FROM resources r WHERE r.id=new.id;
END;
CREATE TRIGGER resources_search_update AFTER UPDATE ON resources BEGIN
  DELETE FROM library_search_fts WHERE source_kind='resource' AND source_id=old.id;
  INSERT INTO library_search_fts(
    source_kind,source_id,article_id,canonical_url,updated_at,
    identity_text,title_text,metadata_text,note_text,excerpt_text,body_text
  )
  SELECT 'resource',r.id,r.linked_article_id,r.canonical_url,r.updated_at,
         r.url||char(10)||r.canonical_url,
         COALESCE(r.title,''),
         trim(COALESCE(r.purpose_zh,'')||char(10)||COALESCE(r.use_when_zh,'')||char(10)||
              r.capabilities||char(10)||r.limitations||char(10)||r.languages||char(10)||
              COALESCE((SELECT group_concat(category,' ') FROM resource_categories c WHERE c.resource_id=r.id),'')||char(10)||
              COALESCE((SELECT group_concat(name,' ') FROM resource_tags t WHERE t.resource_id=r.id),'')),
         COALESCE(r.private_note,''),'',
         COALESCE((SELECT cleaned_content FROM resource_snapshots s WHERE s.id=r.latest_snapshot_id),'')
  FROM resources r WHERE r.id=new.id;
END;
CREATE TRIGGER resources_search_delete AFTER DELETE ON resources BEGIN
  DELETE FROM library_search_fts WHERE source_kind='resource' AND source_id=old.id;
END;

CREATE TRIGGER resource_categories_search_insert AFTER INSERT ON resource_categories BEGIN
  DELETE FROM library_search_fts WHERE source_kind='resource' AND source_id=new.resource_id;
  INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
  SELECT 'resource',r.id,r.linked_article_id,r.canonical_url,r.updated_at,r.url||char(10)||r.canonical_url,COALESCE(r.title,''),
         trim(COALESCE(r.purpose_zh,'')||char(10)||COALESCE(r.use_when_zh,'')||char(10)||r.capabilities||char(10)||r.limitations||char(10)||r.languages||char(10)||COALESCE((SELECT group_concat(category,' ') FROM resource_categories c WHERE c.resource_id=r.id),'')||char(10)||COALESCE((SELECT group_concat(name,' ') FROM resource_tags t WHERE t.resource_id=r.id),'')),
         COALESCE(r.private_note,''),'',COALESCE((SELECT cleaned_content FROM resource_snapshots s WHERE s.id=r.latest_snapshot_id),'')
  FROM resources r WHERE r.id=new.resource_id;
END;
CREATE TRIGGER resource_categories_search_delete AFTER DELETE ON resource_categories BEGIN
  DELETE FROM library_search_fts WHERE source_kind='resource' AND source_id=old.resource_id;
  INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
  SELECT 'resource',r.id,r.linked_article_id,r.canonical_url,r.updated_at,r.url||char(10)||r.canonical_url,COALESCE(r.title,''),
         trim(COALESCE(r.purpose_zh,'')||char(10)||COALESCE(r.use_when_zh,'')||char(10)||r.capabilities||char(10)||r.limitations||char(10)||r.languages||char(10)||COALESCE((SELECT group_concat(category,' ') FROM resource_categories c WHERE c.resource_id=r.id),'')||char(10)||COALESCE((SELECT group_concat(name,' ') FROM resource_tags t WHERE t.resource_id=r.id),'')),
         COALESCE(r.private_note,''),'',COALESCE((SELECT cleaned_content FROM resource_snapshots s WHERE s.id=r.latest_snapshot_id),'')
  FROM resources r WHERE r.id=old.resource_id;
END;
CREATE TRIGGER resource_categories_search_update AFTER UPDATE ON resource_categories BEGIN
  DELETE FROM library_search_fts WHERE source_kind='resource' AND source_id=old.resource_id;
  DELETE FROM library_search_fts WHERE source_kind='resource' AND source_id=new.resource_id;
  INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
  SELECT 'resource',r.id,r.linked_article_id,r.canonical_url,r.updated_at,r.url||char(10)||r.canonical_url,COALESCE(r.title,''),
         trim(COALESCE(r.purpose_zh,'')||char(10)||COALESCE(r.use_when_zh,'')||char(10)||r.capabilities||char(10)||r.limitations||char(10)||r.languages||char(10)||COALESCE((SELECT group_concat(category,' ') FROM resource_categories c WHERE c.resource_id=r.id),'')||char(10)||COALESCE((SELECT group_concat(name,' ') FROM resource_tags t WHERE t.resource_id=r.id),'')),
         COALESCE(r.private_note,''),'',COALESCE((SELECT cleaned_content FROM resource_snapshots s WHERE s.id=r.latest_snapshot_id),'')
  FROM resources r WHERE r.id=new.resource_id;
END;

CREATE TRIGGER resource_tags_search_insert AFTER INSERT ON resource_tags BEGIN
  DELETE FROM library_search_fts WHERE source_kind='resource' AND source_id=new.resource_id;
  INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
  SELECT 'resource',r.id,r.linked_article_id,r.canonical_url,r.updated_at,r.url||char(10)||r.canonical_url,COALESCE(r.title,''),
         trim(COALESCE(r.purpose_zh,'')||char(10)||COALESCE(r.use_when_zh,'')||char(10)||r.capabilities||char(10)||r.limitations||char(10)||r.languages||char(10)||COALESCE((SELECT group_concat(category,' ') FROM resource_categories c WHERE c.resource_id=r.id),'')||char(10)||COALESCE((SELECT group_concat(name,' ') FROM resource_tags t WHERE t.resource_id=r.id),'')),
         COALESCE(r.private_note,''),'',COALESCE((SELECT cleaned_content FROM resource_snapshots s WHERE s.id=r.latest_snapshot_id),'')
  FROM resources r WHERE r.id=new.resource_id;
END;
CREATE TRIGGER resource_tags_search_update AFTER UPDATE ON resource_tags BEGIN
  DELETE FROM library_search_fts WHERE source_kind='resource' AND source_id=old.resource_id;
  DELETE FROM library_search_fts WHERE source_kind='resource' AND source_id=new.resource_id;
  INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
  SELECT 'resource',r.id,r.linked_article_id,r.canonical_url,r.updated_at,r.url||char(10)||r.canonical_url,COALESCE(r.title,''),
         trim(COALESCE(r.purpose_zh,'')||char(10)||COALESCE(r.use_when_zh,'')||char(10)||r.capabilities||char(10)||r.limitations||char(10)||r.languages||char(10)||COALESCE((SELECT group_concat(category,' ') FROM resource_categories c WHERE c.resource_id=r.id),'')||char(10)||COALESCE((SELECT group_concat(name,' ') FROM resource_tags t WHERE t.resource_id=r.id),'')),
         COALESCE(r.private_note,''),'',COALESCE((SELECT cleaned_content FROM resource_snapshots s WHERE s.id=r.latest_snapshot_id),'')
  FROM resources r WHERE r.id=new.resource_id;
END;
CREATE TRIGGER resource_tags_search_delete AFTER DELETE ON resource_tags BEGIN
  DELETE FROM library_search_fts WHERE source_kind='resource' AND source_id=old.resource_id;
  INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
  SELECT 'resource',r.id,r.linked_article_id,r.canonical_url,r.updated_at,r.url||char(10)||r.canonical_url,COALESCE(r.title,''),
         trim(COALESCE(r.purpose_zh,'')||char(10)||COALESCE(r.use_when_zh,'')||char(10)||r.capabilities||char(10)||r.limitations||char(10)||r.languages||char(10)||COALESCE((SELECT group_concat(category,' ') FROM resource_categories c WHERE c.resource_id=r.id),'')||char(10)||COALESCE((SELECT group_concat(name,' ') FROM resource_tags t WHERE t.resource_id=r.id),'')),
         COALESCE(r.private_note,''),'',COALESCE((SELECT cleaned_content FROM resource_snapshots s WHERE s.id=r.latest_snapshot_id),'')
  FROM resources r WHERE r.id=old.resource_id;
END;

CREATE TRIGGER articles_search_insert AFTER INSERT ON articles BEGIN
  INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
  SELECT 'article',a.id,a.id,COALESCE(a.url,''),COALESCE(a.published,a.fetched_at),COALESCE(a.url,'')||char(10)||f.url,COALESCE(a.title,''),
         trim(COALESCE(a.author,'')||char(10)||COALESCE((SELECT group_concat(t.name,' ') FROM article_tags at JOIN tags t ON t.id=at.tag_id WHERE at.article_id=a.id),'')),
         '','',COALESCE(a.content,'') FROM articles a LEFT JOIN feeds f ON f.id=a.feed_id WHERE a.id=new.id;
END;
CREATE TRIGGER articles_search_update AFTER UPDATE OF title,author,content,url,feed_id,published,fetched_at ON articles BEGIN
  DELETE FROM library_search_fts WHERE source_kind='article' AND source_id=old.id;
  INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
  SELECT 'article',a.id,a.id,COALESCE(a.url,''),COALESCE(a.published,a.fetched_at),COALESCE(a.url,'')||char(10)||f.url,COALESCE(a.title,''),
         trim(COALESCE(a.author,'')||char(10)||COALESCE((SELECT group_concat(t.name,' ') FROM article_tags at JOIN tags t ON t.id=at.tag_id WHERE at.article_id=a.id),'')),
         '','',COALESCE(a.content,'') FROM articles a LEFT JOIN feeds f ON f.id=a.feed_id WHERE a.id=new.id;
END;
CREATE TRIGGER articles_search_delete AFTER DELETE ON articles BEGIN
  DELETE FROM library_search_fts WHERE article_id=old.id;
END;

CREATE TRIGGER article_tags_search_insert AFTER INSERT ON article_tags BEGIN
  DELETE FROM library_search_fts WHERE source_kind='article' AND source_id=new.article_id;
  INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
  SELECT 'article',a.id,a.id,COALESCE(a.url,''),COALESCE(a.published,a.fetched_at),COALESCE(a.url,'')||char(10)||f.url,COALESCE(a.title,''),
         trim(COALESCE(a.author,'')||char(10)||COALESCE((SELECT group_concat(t.name,' ') FROM article_tags at JOIN tags t ON t.id=at.tag_id WHERE at.article_id=a.id),'')),
         '','',COALESCE(a.content,'') FROM articles a LEFT JOIN feeds f ON f.id=a.feed_id WHERE a.id=new.article_id;
END;
CREATE TRIGGER article_tags_search_delete AFTER DELETE ON article_tags BEGIN
  DELETE FROM library_search_fts WHERE source_kind='article' AND source_id=old.article_id;
  INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
  SELECT 'article',a.id,a.id,COALESCE(a.url,''),COALESCE(a.published,a.fetched_at),COALESCE(a.url,'')||char(10)||f.url,COALESCE(a.title,''),
         trim(COALESCE(a.author,'')||char(10)||COALESCE((SELECT group_concat(t.name,' ') FROM article_tags at JOIN tags t ON t.id=at.tag_id WHERE at.article_id=a.id),'')),
         '','',COALESCE(a.content,'') FROM articles a LEFT JOIN feeds f ON f.id=a.feed_id WHERE a.id=old.article_id;
END;

CREATE TRIGGER selections_search_insert AFTER INSERT ON article_selections BEGIN
  INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
    SELECT 'excerpt',new.id,new.article_id,'',new.updated_at,'','','','',new.selected_text,'' WHERE new.is_favorite=1;
  INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
    SELECT 'thought',new.id,new.article_id,'',new.updated_at,'','','',new.comment,'','' WHERE new.comment IS NOT NULL AND length(trim(new.comment))>0;
END;
CREATE TRIGGER selections_search_update AFTER UPDATE ON article_selections BEGIN
  DELETE FROM library_search_fts WHERE source_kind IN ('excerpt','thought') AND source_id=old.id;
  INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
    SELECT 'excerpt',new.id,new.article_id,'',new.updated_at,'','','','',new.selected_text,'' WHERE new.is_favorite=1;
  INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
    SELECT 'thought',new.id,new.article_id,'',new.updated_at,'','','',new.comment,'','' WHERE new.comment IS NOT NULL AND length(trim(new.comment))>0;
END;
CREATE TRIGGER selections_search_delete AFTER DELETE ON article_selections BEGIN
  DELETE FROM library_search_fts WHERE source_kind IN ('excerpt','thought') AND source_id=old.id;
END;
"#;

const INDEX_BACKFILL: &str = r#"
INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
SELECT 'resource',r.id,r.linked_article_id,r.canonical_url,r.updated_at,r.url||char(10)||r.canonical_url,COALESCE(r.title,''),
       trim(COALESCE(r.purpose_zh,'')||char(10)||COALESCE(r.use_when_zh,'')||char(10)||r.capabilities||char(10)||r.limitations||char(10)||r.languages||char(10)||COALESCE((SELECT group_concat(category,' ') FROM resource_categories c WHERE c.resource_id=r.id),'')||char(10)||COALESCE((SELECT group_concat(name,' ') FROM resource_tags t WHERE t.resource_id=r.id),'')),
       COALESCE(r.private_note,''),'',COALESCE((SELECT cleaned_content FROM resource_snapshots s WHERE s.id=r.latest_snapshot_id),'')
FROM resources r;
INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
SELECT 'article',a.id,a.id,COALESCE(a.url,''),COALESCE(a.published,a.fetched_at),COALESCE(a.url,'')||char(10)||f.url,COALESCE(a.title,''),
       trim(COALESCE(a.author,'')||char(10)||COALESCE((SELECT group_concat(t.name,' ') FROM article_tags at JOIN tags t ON t.id=at.tag_id WHERE at.article_id=a.id),'')),
       '','',COALESCE(a.content,'') FROM articles a LEFT JOIN feeds f ON f.id=a.feed_id;
INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
SELECT 'excerpt',s.id,s.article_id,'',s.updated_at,'','','','',s.selected_text,'' FROM article_selections s WHERE s.is_favorite=1;
INSERT INTO library_search_fts(source_kind,source_id,article_id,canonical_url,updated_at,identity_text,title_text,metadata_text,note_text,excerpt_text,body_text)
SELECT 'thought',s.id,s.article_id,'',s.updated_at,'','','',s.comment,'','' FROM article_selections s WHERE s.comment IS NOT NULL AND length(trim(s.comment))>0;
"#;

pub(crate) fn migrate_to_v5(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS articles_fts_insert;
         DROP TRIGGER IF EXISTS articles_fts_update;
         DROP TRIGGER IF EXISTS articles_fts_delete;
         DROP TRIGGER IF EXISTS selections_fts_insert;
         DROP TRIGGER IF EXISTS selections_fts_update;
         DROP TRIGGER IF EXISTS selections_fts_delete;
         DROP TRIGGER IF EXISTS article_tags_fts_insert;
         DROP TRIGGER IF EXISTS article_tags_fts_delete;
         DROP TRIGGER IF EXISTS resources_search_insert;
         DROP TRIGGER IF EXISTS resources_search_update;
         DROP TRIGGER IF EXISTS resources_search_delete;
         DROP TRIGGER IF EXISTS resource_categories_search_insert;
         DROP TRIGGER IF EXISTS resource_categories_search_update;
         DROP TRIGGER IF EXISTS resource_categories_search_delete;
         DROP TRIGGER IF EXISTS resource_tags_search_insert;
         DROP TRIGGER IF EXISTS resource_tags_search_update;
         DROP TRIGGER IF EXISTS resource_tags_search_delete;
         DROP TRIGGER IF EXISTS articles_search_insert;
         DROP TRIGGER IF EXISTS articles_search_update;
         DROP TRIGGER IF EXISTS articles_search_delete;
         DROP TRIGGER IF EXISTS article_tags_search_insert;
         DROP TRIGGER IF EXISTS article_tags_search_delete;
         DROP TRIGGER IF EXISTS selections_search_insert;
         DROP TRIGGER IF EXISTS selections_search_update;
         DROP TRIGGER IF EXISTS selections_search_delete;
         DROP TABLE IF EXISTS library_fts;
         DROP TABLE IF EXISTS resource_fts;
         DROP TABLE IF EXISTS library_search_fts;",
    )?;
    conn.execute_batch(SEARCH_INDEX_SCHEMA)?;
    conn.execute_batch(INDEX_BACKFILL)?;
    verify_index(conn)?;
    Ok(())
}

pub(crate) fn verify_index(conn: &Connection) -> anyhow::Result<()> {
    let expected: i64 = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM resources) +
                (SELECT COUNT(*) FROM articles) +
                (SELECT COUNT(*) FROM article_selections WHERE is_favorite=1) +
                (SELECT COUNT(*) FROM article_selections WHERE comment IS NOT NULL AND length(trim(comment))>0)",
        [],
        |row| row.get(0),
    )?;
    let actual: i64 = conn.query_row("SELECT COUNT(*) FROM library_search_fts", [], |row| {
        row.get(0)
    })?;
    anyhow::ensure!(
        actual == expected,
        "LIBRARY_SEARCH_INDEX_COUNT_MISMATCH: expected {expected}, got {actual}"
    );
    Ok(())
}

pub(crate) fn verify_schema(conn: &Connection) -> anyhow::Result<()> {
    let table_exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='library_search_fts')",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(table_exists, "LIBRARY_SEARCH_INDEX_MISSING");

    for trigger in [
        "resources_search_insert",
        "resources_search_update",
        "resources_search_delete",
        "resource_categories_search_insert",
        "resource_categories_search_update",
        "resource_categories_search_delete",
        "resource_tags_search_insert",
        "resource_tags_search_update",
        "resource_tags_search_delete",
        "articles_search_insert",
        "articles_search_update",
        "articles_search_delete",
        "article_tags_search_insert",
        "article_tags_search_delete",
        "selections_search_insert",
        "selections_search_update",
        "selections_search_delete",
    ] {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='trigger' AND name=?1)",
            [trigger],
            |row| row.get(0),
        )?;
        anyhow::ensure!(exists, "LIBRARY_SEARCH_TRIGGER_MISSING: {trigger}");
    }
    verify_index(conn)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchScope {
    Curated,
    AllArticles,
    Archive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResultType {
    All,
    Resource,
    Article,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchOrigin {
    Human,
    Agent,
}

#[derive(Debug, Clone)]
pub(crate) struct SearchRequest {
    pub(crate) query: String,
    pub(crate) scope: SearchScope,
    pub(crate) result_type: ResultType,
    pub(crate) origin: SearchOrigin,
    pub(crate) limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum PrimaryIdentity {
    Resource(i64),
    Article(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum EvidenceKind {
    Resource,
    Article,
    WebClipping,
    Excerpt,
    Thought,
}

impl EvidenceKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Resource => "resource",
            Self::Article => "article",
            Self::WebClipping => "web_clipping",
            Self::Excerpt => "excerpt",
            Self::Thought => "thought",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum EvidenceField {
    Identity,
    Title,
    Metadata,
    PrivateNote,
    Excerpt,
    Body,
}

impl EvidenceField {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Title => "title",
            Self::Metadata => "metadata",
            Self::PrivateNote => "private_note",
            Self::Excerpt => "excerpt",
            Self::Body => "body",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchEvidence {
    pub(crate) kind: EvidenceKind,
    pub(crate) source_id: i64,
    pub(crate) article_id: Option<i64>,
    pub(crate) field: EvidenceField,
    pub(crate) text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum ScoreFactor {
    ExactIdentity,
    ExactTitle,
    StrongestEvidence,
    Corroborated,
    ManualRating,
    BrokenPenalty,
}

impl ScoreFactor {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ExactIdentity => "exact_identity",
            Self::ExactTitle => "exact_title",
            Self::StrongestEvidence => "strongest_evidence",
            Self::Corroborated => "corroborated",
            Self::ManualRating => "manual_rating",
            Self::BrokenPenalty => "broken_penalty",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArticleTarget {
    pub(crate) article_id: i64,
    pub(crate) feed_id: i64,
    pub(crate) selection_id: Option<i64>,
    pub(crate) archived: bool,
    pub(crate) web_clipping: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LibrarySearchResult {
    pub(crate) primary: PrimaryIdentity,
    pub(crate) title: Option<String>,
    pub(crate) url: Option<String>,
    pub(crate) privacy: Option<String>,
    pub(crate) health: Option<String>,
    pub(crate) archived: bool,
    pub(crate) updated_at: i64,
    pub(crate) evidence: Vec<SearchEvidence>,
    pub(crate) factors: Vec<ScoreFactor>,
    pub(crate) article_targets: Vec<ArticleTarget>,
    score: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SearchWarning {
    HistoryNotRecorded { technical_detail: String },
}

#[derive(Debug, Clone)]
pub(crate) struct SearchOutcome {
    pub(crate) query: String,
    pub(crate) results: Vec<LibrarySearchResult>,
    pub(crate) warnings: Vec<SearchWarning>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureKind {
    Input,
    Maintenance,
    Storage,
    Index,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{user_message}")]
pub(crate) struct SearchFailure {
    pub(crate) kind: FailureKind,
    pub(crate) user_message: String,
    pub(crate) technical_detail: String,
}

impl SearchFailure {
    fn input(detail: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Input,
            user_message: "请输入有效的搜索内容".into(),
            technical_detail: detail.into(),
        }
    }

    fn from_storage(error: impl fmt::Display) -> Self {
        let detail = error.to_string();
        let (kind, message) = if detail.contains("MAINTENANCE_IN_PROGRESS")
            || detail.contains("STALE_LIBRARY_EPOCH")
        {
            (FailureKind::Maintenance, "资料维护期间暂时无法搜索")
        } else if detail.contains("library_search_fts") || detail.contains("LIBRARY_SEARCH_INDEX") {
            (FailureKind::Index, "资料库搜索索引不可用，请检查资料库")
        } else if detail.contains("interrupted") {
            (FailureKind::Storage, "搜索超过两秒，已停止")
        } else {
            (FailureKind::Storage, "资料库搜索失败")
        };
        Self {
            kind,
            user_message: message.into(),
            technical_detail: detail,
        }
    }
}

pub(crate) struct LibrarySearch<'db> {
    db: &'db Db,
    budget: Duration,
}

impl<'db> LibrarySearch<'db> {
    pub(crate) fn new(db: &'db Db) -> Self {
        Self {
            db,
            budget: DEFAULT_BUDGET,
        }
    }

    #[cfg(test)]
    fn with_budget(db: &'db Db, budget: Duration) -> Self {
        Self { db, budget }
    }

    pub(crate) fn search(&self, request: SearchRequest) -> Result<SearchOutcome, SearchFailure> {
        let original_query = request.query.trim().to_owned();
        if original_query.is_empty() {
            return Err(SearchFailure::input("EMPTY_LIBRARY_SEARCH_QUERY"));
        }
        if !(1..=MAX_RESULTS).contains(&request.limit) {
            return Err(SearchFailure::input(format!(
                "INVALID_LIBRARY_SEARCH_LIMIT: {}",
                request.limit
            )));
        }
        let normalized_query = normalize(&original_query);
        if self.budget.is_zero() {
            return Err(SearchFailure::from_storage(
                "interrupted: LIBRARY_SEARCH_BUDGET_EXPIRED",
            ));
        }
        let deadline = Instant::now() + self.budget;
        self.db
            .conn
            .progress_handler(1_000, Some(move || Instant::now() >= deadline))
            .map_err(SearchFailure::from_storage)?;
        let searched = self.search_inner(&request, &normalized_query, deadline);
        let _ = self.db.conn.progress_handler(0, None::<fn() -> bool>);
        let mut results = searched.map_err(SearchFailure::from_storage)?;
        results.truncate(request.limit);
        let mut warnings = Vec::new();
        if request.origin == SearchOrigin::Human
            && let Err(error) = self.record_history(&original_query, results.len())
        {
            warnings.push(SearchWarning::HistoryNotRecorded {
                technical_detail: error.to_string(),
            });
        }
        Ok(SearchOutcome {
            query: original_query,
            results,
            warnings,
        })
    }

    pub(crate) fn history(&self, limit: usize) -> Result<Vec<SearchHistoryEntry>, SearchFailure> {
        let mut stmt = self
            .db
            .conn
            .prepare(
                "SELECT query,last_used_at,use_count,result_count FROM search_history
                 ORDER BY last_used_at DESC,query LIMIT ?1",
            )
            .map_err(SearchFailure::from_storage)?;
        let rows = stmt
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                Ok(SearchHistoryEntry {
                    query: row.get(0)?,
                    last_used_at: row.get(1)?,
                    use_count: row.get(2)?,
                    result_count: row.get(3)?,
                })
            })
            .map_err(SearchFailure::from_storage)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(SearchFailure::from_storage)
    }

    pub(crate) fn clear_history(&self) -> Result<usize, SearchFailure> {
        let tx = self
            .db
            .fenced_transaction()
            .map_err(SearchFailure::from_storage)?;
        let changed = tx
            .execute("DELETE FROM search_history", [])
            .map_err(SearchFailure::from_storage)?;
        tx.commit().map_err(SearchFailure::from_storage)?;
        Ok(changed)
    }

    fn record_history(&self, query: &str, result_count: usize) -> anyhow::Result<()> {
        let tx = self.db.fenced_transaction()?;
        tx.execute(
            "INSERT INTO search_history(query,last_used_at,use_count,result_count)
             VALUES(?1,unixepoch(),1,?2)
             ON CONFLICT(query) DO UPDATE SET
               last_used_at=excluded.last_used_at,
               use_count=search_history.use_count+1,
               result_count=excluded.result_count",
            params![query, i64::try_from(result_count).unwrap_or(i64::MAX)],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn search_inner(
        &self,
        request: &SearchRequest,
        normalized_query: &str,
        deadline: Instant,
    ) -> anyhow::Result<Vec<LibrarySearchResult>> {
        let terms = normalized_query
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let documents = self.load_documents(&terms, request.limit)?;
        anyhow::ensure!(
            Instant::now() < deadline,
            "interrupted: LIBRARY_SEARCH_BUDGET_EXPIRED"
        );
        let resources = load_resource_facts(&self.db.conn)?;
        let articles = load_article_facts(&self.db.conn)?;
        anyhow::ensure!(
            Instant::now() < deadline,
            "interrupted: LIBRARY_SEARCH_BUDGET_EXPIRED"
        );
        let eligible_resources = eligible_resource_maps(&resources, request.scope, request.origin);
        let mut groups: HashMap<PrimaryIdentity, GroupBuilder> = HashMap::new();

        for document in documents {
            anyhow::ensure!(
                Instant::now() < deadline,
                "interrupted: LIBRARY_SEARCH_BUDGET_EXPIRED"
            );
            let Some(scored) = score_document(&document, normalized_query, &terms) else {
                continue;
            };
            let placement = place_document(
                &document,
                request,
                &resources,
                &articles,
                &eligible_resources,
            );
            let Some(placement) = placement else { continue };
            let group = groups
                .entry(placement.primary)
                .or_insert_with(|| GroupBuilder::from_placement(&placement, &resources, &articles));
            group.absorb(document, scored, placement.article_target);
        }

        let mut results = groups
            .into_values()
            .map(GroupBuilder::finish)
            .collect::<Vec<_>>();
        results.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| right.updated_at.cmp(&left.updated_at))
                .then_with(|| left.primary.cmp(&right.primary))
        });
        Ok(results)
    }

    fn load_documents(&self, terms: &[String], limit: usize) -> anyhow::Result<Vec<RawDocument>> {
        let candidate_limit = limit.saturating_mul(50).clamp(1_000, 10_000);
        let long_terms = terms
            .iter()
            .filter(|term| term.chars().count() >= 3)
            .collect::<Vec<_>>();
        if long_terms.len() == terms.len() {
            let match_query = long_terms
                .iter()
                .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" AND ");
            let mut stmt = self.db.conn.prepare(
                "SELECT source_kind,source_id,article_id,canonical_url,updated_at,
                        identity_text,title_text,metadata_text,note_text,excerpt_text,body_text
                 FROM library_search_fts WHERE library_search_fts MATCH ?1
                 ORDER BY bm25(library_search_fts,9.0,8.0,5.0,6.0,4.0,1.0)
                 LIMIT ?2",
            )?;
            let rows = stmt.query_map(
                params![
                    match_query,
                    i64::try_from(candidate_limit).unwrap_or(i64::MAX)
                ],
                map_document,
            )?;
            return Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }

        let searchable = "lower(identity_text||char(10)||title_text||char(10)||metadata_text||char(10)||note_text||char(10)||excerpt_text||char(10)||body_text)";
        let predicates = (1..=terms.len())
            .map(|index| format!("instr({searchable},lower(?{index}))>0"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let limit_parameter = terms.len() + 1;
        let sql = format!(
            "SELECT source_kind,source_id,article_id,canonical_url,updated_at,
                    identity_text,title_text,metadata_text,note_text,excerpt_text,body_text
             FROM library_search_fts
             WHERE {predicates}
             ORDER BY updated_at DESC LIMIT ?{limit_parameter}"
        );
        let mut values = terms.to_vec();
        values.push(candidate_limit.to_string());
        let mut stmt = self.db.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(values), map_document)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[derive(Debug)]
struct RawDocument {
    source_kind: EvidenceKind,
    source_id: i64,
    article_id: Option<i64>,
    updated_at: i64,
    identity: String,
    title: String,
    metadata: String,
    note: String,
    excerpt: String,
    body: String,
}

fn map_document(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawDocument> {
    let source: String = row.get(0)?;
    let source_kind = match source.as_str() {
        "resource" => EvidenceKind::Resource,
        "article" => EvidenceKind::Article,
        "excerpt" => EvidenceKind::Excerpt,
        "thought" => EvidenceKind::Thought,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(RawDocument {
        source_kind,
        source_id: row.get(1)?,
        article_id: row.get(2)?,
        updated_at: row.get(4)?,
        identity: row.get(5)?,
        title: row.get(6)?,
        metadata: row.get(7)?,
        note: row.get(8)?,
        excerpt: row.get(9)?,
        body: row.get(10)?,
    })
}

#[derive(Debug)]
struct ResourceFact {
    id: i64,
    title: Option<String>,
    url: String,
    canonical_url: String,
    linked_article_id: Option<i64>,
    privacy: String,
    health: String,
    curation: String,
    rating: Option<i64>,
    updated_at: i64,
}

#[derive(Debug)]
struct ArticleFact {
    id: i64,
    feed_id: i64,
    title: Option<String>,
    url: Option<String>,
    starred: bool,
    archived: bool,
    updated_at: i64,
    web_clipping: bool,
}

fn load_resource_facts(conn: &Connection) -> rusqlite::Result<HashMap<i64, ResourceFact>> {
    let mut stmt = conn.prepare(
        "SELECT id,title,url,canonical_url,linked_article_id,privacy,health,curation_state,manual_rating,updated_at FROM resources",
    )?;
    let rows = stmt.query_map([], |row| {
        let fact = ResourceFact {
            id: row.get(0)?,
            title: row.get(1)?,
            url: row.get(2)?,
            canonical_url: row.get(3)?,
            linked_article_id: row.get(4)?,
            privacy: row.get(5)?,
            health: row.get(6)?,
            curation: row.get(7)?,
            rating: row.get(8)?,
            updated_at: row.get(9)?,
        };
        Ok((fact.id, fact))
    })?;
    rows.collect()
}

fn load_article_facts(conn: &Connection) -> rusqlite::Result<HashMap<i64, ArticleFact>> {
    let mut stmt = conn.prepare(
        "SELECT a.id,a.feed_id,a.title,a.url,a.starred,a.archived,COALESCE(a.published,a.fetched_at),f.url
         FROM articles a JOIN feeds f ON f.id=a.feed_id",
    )?;
    let rows = stmt.query_map([], |row| {
        let feed_url: String = row.get(7)?;
        let fact = ArticleFact {
            id: row.get(0)?,
            feed_id: row.get(1)?,
            title: row.get(2)?,
            url: row.get(3)?,
            starred: row.get(4)?,
            archived: row.get(5)?,
            updated_at: row.get(6)?,
            web_clipping: feed_url == WEB_CLIPPINGS_FEED_URL,
        };
        Ok((fact.id, fact))
    })?;
    rows.collect()
}

struct EligibleResourceMaps {
    by_linked_article: HashMap<i64, i64>,
    by_canonical_url: HashMap<String, i64>,
}

fn eligible_resource_maps(
    resources: &HashMap<i64, ResourceFact>,
    scope: SearchScope,
    origin: SearchOrigin,
) -> EligibleResourceMaps {
    let mut by_linked_article = HashMap::new();
    let mut by_canonical_url = HashMap::new();
    for resource in resources.values().filter(|resource| {
        let visible = match scope {
            SearchScope::Curated | SearchScope::AllArticles => resource.curation == "active",
            SearchScope::Archive => resource.curation == "archived",
        };
        visible && !(origin == SearchOrigin::Agent && resource.privacy == "private")
    }) {
        if let Some(article_id) = resource.linked_article_id {
            by_linked_article.insert(article_id, resource.id);
        }
        by_canonical_url.insert(resource.canonical_url.clone(), resource.id);
    }
    EligibleResourceMaps {
        by_linked_article,
        by_canonical_url,
    }
}

struct Placement {
    primary: PrimaryIdentity,
    article_target: Option<ArticleTarget>,
}

fn place_document(
    document: &RawDocument,
    request: &SearchRequest,
    resources: &HashMap<i64, ResourceFact>,
    articles: &HashMap<i64, ArticleFact>,
    eligible_resources: &EligibleResourceMaps,
) -> Option<Placement> {
    if document.source_kind == EvidenceKind::Resource {
        let resource = resources.get(&document.source_id)?;
        let eligible = match request.scope {
            SearchScope::Curated | SearchScope::AllArticles => resource.curation == "active",
            SearchScope::Archive => resource.curation == "archived",
        };
        if !eligible
            || (request.origin == SearchOrigin::Agent && resource.privacy == "private")
            || request.result_type == ResultType::Article
        {
            return None;
        }
        return Some(Placement {
            primary: PrimaryIdentity::Resource(resource.id),
            article_target: None,
        });
    }

    let article_id = document.article_id?;
    let article = articles.get(&article_id)?;
    let canonical = article
        .url
        .as_deref()
        .and_then(|url| canonicalize_url(url).ok());
    let resource_id = eligible_resources
        .by_linked_article
        .get(&article_id)
        .copied()
        .or_else(|| {
            canonical
                .as_ref()
                .and_then(|url| eligible_resources.by_canonical_url.get(url).copied())
        });
    let primary = resource_id
        .map(PrimaryIdentity::Resource)
        .unwrap_or(PrimaryIdentity::Article(article_id));
    if matches!(request.result_type, ResultType::Resource)
        && !matches!(primary, PrimaryIdentity::Resource(_))
    {
        return None;
    }
    if matches!(request.result_type, ResultType::Article)
        && !matches!(primary, PrimaryIdentity::Article(_))
    {
        return None;
    }
    let primary_material_eligible = match request.scope {
        SearchScope::Curated => !article.archived && (article.starred || article.web_clipping),
        SearchScope::AllArticles => !article.archived,
        SearchScope::Archive => article.archived,
    };
    let note_evidence = matches!(
        document.source_kind,
        EvidenceKind::Excerpt | EvidenceKind::Thought
    );
    if !(primary_material_eligible || note_evidence && request.scope != SearchScope::Archive) {
        return None;
    }
    Some(Placement {
        primary,
        article_target: Some(ArticleTarget {
            article_id,
            feed_id: article.feed_id,
            selection_id: note_evidence.then_some(document.source_id),
            archived: article.archived,
            web_clipping: article.web_clipping,
        }),
    })
}

struct ScoredDocument {
    score: i64,
    evidence: Vec<(EvidenceField, String)>,
    factors: BTreeSet<ScoreFactor>,
}

fn score_document(
    document: &RawDocument,
    normalized_query: &str,
    terms: &[String],
) -> Option<ScoredDocument> {
    let fields = [
        (EvidenceField::Identity, &document.identity, 8_000),
        (EvidenceField::Title, &document.title, 7_000),
        (EvidenceField::Metadata, &document.metadata, 5_000),
        (EvidenceField::PrivateNote, &document.note, 4_500),
        (EvidenceField::Excerpt, &document.excerpt, 3_500),
        (EvidenceField::Body, &document.body, 1_000),
    ];
    let normalized_fields = fields
        .iter()
        .map(|(field, text, weight)| (*field, normalize(text), *text, *weight))
        .collect::<Vec<_>>();
    if terms.iter().any(|term| {
        !normalized_fields
            .iter()
            .any(|(_, normalized, _, _)| normalized.contains(term))
    }) {
        return None;
    }
    let mut evidence = Vec::new();
    let mut strongest = 0;
    let mut factors = BTreeSet::from([ScoreFactor::StrongestEvidence]);
    for (field, normalized, original, weight) in normalized_fields {
        if normalized.contains(normalized_query)
            || terms.iter().any(|term| normalized.contains(term))
        {
            strongest = strongest.max(weight);
            evidence.push((field, evidence_snippet(original, terms, 220)));
            if field == EvidenceField::Identity && normalized.trim() == normalized_query {
                strongest = strongest.max(10_000);
                factors.insert(ScoreFactor::ExactIdentity);
            }
            if field == EvidenceField::Title && normalized.trim() == normalized_query {
                strongest = strongest.max(9_000);
                factors.insert(ScoreFactor::ExactTitle);
            }
        }
    }
    if evidence.len() > 1 {
        factors.insert(ScoreFactor::Corroborated);
    }
    let corroboration = i64::try_from(evidence.len().saturating_sub(1))
        .unwrap_or(i64::MAX)
        .saturating_mul(150)
        .min(600);
    Some(ScoredDocument {
        score: strongest + corroboration,
        evidence,
        factors,
    })
}

fn normalize(value: &str) -> String {
    value
        .nfkc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn evidence_snippet(value: &str, terms: &[String], max_chars: usize) -> String {
    let plain = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let normalized = normalize(&plain);
    let first = terms
        .iter()
        .filter_map(|term| normalized.find(term))
        .min()
        .unwrap_or(0);
    let char_start = normalized[..first]
        .chars()
        .count()
        .saturating_sub(max_chars / 3);
    let chars = plain.chars().collect::<Vec<_>>();
    let end = char_start.saturating_add(max_chars).min(chars.len());
    let mut snippet = chars[char_start.min(chars.len())..end]
        .iter()
        .collect::<String>();
    if char_start > 0 {
        snippet.insert(0, '…');
    }
    if end < chars.len() {
        snippet.push('…');
    }
    snippet
}

struct GroupBuilder {
    result: LibrarySearchResult,
    strongest: i64,
    rating: Option<i64>,
}

impl GroupBuilder {
    fn from_placement(
        placement: &Placement,
        resources: &HashMap<i64, ResourceFact>,
        articles: &HashMap<i64, ArticleFact>,
    ) -> Self {
        let (result, rating) = match placement.primary {
            PrimaryIdentity::Resource(id) => {
                let resource = &resources[&id];
                (
                    LibrarySearchResult {
                        primary: placement.primary,
                        title: resource.title.clone(),
                        url: Some(resource.url.clone()),
                        privacy: Some(resource.privacy.clone()),
                        health: Some(resource.health.clone()),
                        archived: resource.curation == "archived",
                        updated_at: resource.updated_at,
                        evidence: Vec::new(),
                        factors: Vec::new(),
                        article_targets: Vec::new(),
                        score: 0,
                    },
                    resource.rating,
                )
            }
            PrimaryIdentity::Article(id) => {
                let article = &articles[&id];
                (
                    LibrarySearchResult {
                        primary: placement.primary,
                        title: article.title.clone(),
                        url: article.url.clone(),
                        privacy: None,
                        health: None,
                        archived: article.archived,
                        updated_at: article.updated_at,
                        evidence: Vec::new(),
                        factors: Vec::new(),
                        article_targets: Vec::new(),
                        score: 0,
                    },
                    None,
                )
            }
        };
        Self {
            result,
            strongest: 0,
            rating,
        }
    }

    fn absorb(
        &mut self,
        document: RawDocument,
        scored: ScoredDocument,
        article_target: Option<ArticleTarget>,
    ) {
        self.strongest = self.strongest.max(scored.score);
        self.result.updated_at = self.result.updated_at.max(document.updated_at);
        let kind = match document.source_kind {
            EvidenceKind::Article
                if article_target
                    .as_ref()
                    .is_some_and(|target| target.web_clipping) =>
            {
                EvidenceKind::WebClipping
            }
            kind => kind,
        };
        for (field, text) in scored.evidence {
            if self.result.evidence.len() >= MAX_EVIDENCE {
                break;
            }
            let evidence = SearchEvidence {
                kind,
                source_id: document.source_id,
                article_id: document.article_id,
                field,
                text,
            };
            if !self.result.evidence.contains(&evidence) {
                self.result.evidence.push(evidence);
            }
        }
        for factor in scored.factors {
            if !self.result.factors.contains(&factor) {
                self.result.factors.push(factor);
            }
        }
        if let Some(target) = article_target
            && !self.result.article_targets.contains(&target)
        {
            self.result.article_targets.push(target);
        }
    }

    fn finish(mut self) -> LibrarySearchResult {
        let corroboration = i64::try_from(self.result.evidence.len().saturating_sub(1))
            .unwrap_or(i64::MAX)
            .saturating_mul(150)
            .min(600);
        let mut score = self.strongest + corroboration;
        if corroboration > 0 {
            self.result.factors.push(ScoreFactor::Corroborated);
        }
        if let Some(rating) = self.rating {
            score += rating * 40;
            self.result.factors.push(ScoreFactor::ManualRating);
        }
        if self.result.health.as_deref() == Some("broken") {
            score -= 100;
            self.result.factors.push(ScoreFactor::BrokenPenalty);
        }
        self.result.factors.sort();
        self.result.factors.dedup();
        self.result.article_targets.sort_by_key(|target| {
            (
                target.archived,
                !target.web_clipping,
                target.selection_id.is_none(),
                target.article_id,
            )
        });
        self.result.score = score;
        self.result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::BackupStore;
    use crate::local_data_maintenance::{MaintenanceEngine, MaintenanceRequest, MaintenanceStatus};
    use crate::resource_library_lifecycle::{
        Category, CompleteManualEdit, CreateResource, ProcessingHandoff, ProjectionScope,
        ResourceCurationState, ResourceKind, ResourceLibraryLifecycle, ResourceLifecycleChange,
        ResourcePrivacy, ResourceSource, ResourceTag, TagLanguage, TagSource,
    };

    #[derive(Debug)]
    struct FixedClock(i64);
    impl crate::resource_library_lifecycle::Clock for FixedClock {
        fn now(&self) -> i64 {
            self.0
        }
    }
    #[derive(Debug)]
    struct NoHandoff;
    impl ProcessingHandoff for NoHandoff {
        fn request_resource_processing(&self, _resource_id: i64) -> Result<(), String> {
            Ok(())
        }
    }

    fn memory_db() -> Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::schema_evolution::evolve(&conn).unwrap();
        Db {
            conn,
            path: None,
            _maintenance_fence: None,
        }
    }

    #[test]
    fn resource_and_linked_article_share_one_ranking_position() {
        let db = memory_db();
        let feed_id = db.add_feed("https://example.test/feed", 1).unwrap();
        db.conn.execute("INSERT INTO articles(feed_id,entry_id,url,title,content,starred,fetched_at) VALUES(?1,'a','https://example.test/tool','Tool article','special architecture tool',1,2)", [feed_id]).unwrap();
        let article_id = db.conn.last_insert_rowid();
        let lifecycle = ResourceLibraryLifecycle::new(&db, &NoHandoff, &FixedClock(3));
        let outcome = lifecycle
            .apply(
                ResourceLifecycleChange::Create(CreateResource {
                    url: "https://example.test/tool".into(),
                    parent_resource_id: None,
                    linked_article_id: Some(article_id),
                    kind: ResourceKind::Site,
                    title: Some("Architecture Tool".into()),
                    private_note: None,
                    privacy: ResourcePrivacy::Public,
                    source: ResourceSource::Gui,
                    manual_rating: Some(5),
                }),
                ProjectionScope::Resource(1),
            )
            .unwrap();
        let resource_id = outcome.affected_resource_ids[0];
        let outcome = LibrarySearch::new(&db)
            .search(SearchRequest {
                query: "architecture tool".into(),
                scope: SearchScope::Curated,
                result_type: ResultType::All,
                origin: SearchOrigin::Agent,
                limit: 5,
            })
            .unwrap();
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(
            outcome.results[0].primary,
            PrimaryIdentity::Resource(resource_id)
        );
        assert_eq!(outcome.results[0].article_targets[0].article_id, article_id);
    }

    #[test]
    fn agent_search_does_not_pollute_human_history() {
        let db = memory_db();
        let search = LibrarySearch::new(&db);
        let outcome = search
            .search(SearchRequest {
                query: "missing".into(),
                scope: SearchScope::Curated,
                result_type: ResultType::All,
                origin: SearchOrigin::Agent,
                limit: 5,
            })
            .unwrap();
        assert!(outcome.results.is_empty());
        assert!(search.history(10).unwrap().is_empty());
        search
            .search(SearchRequest {
                query: "missing".into(),
                scope: SearchScope::Curated,
                result_type: ResultType::All,
                origin: SearchOrigin::Human,
                limit: 5,
            })
            .unwrap();
        assert_eq!(search.history(10).unwrap()[0].query, "missing");
    }

    #[test]
    fn search_history_mutations_respect_the_maintenance_fence() {
        let root = std::env::temp_dir().join(format!(
            "rrss-search-history-fence-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("library.sqlite3");
        let db = Db::open(&path).unwrap();
        let search = LibrarySearch::new(&db);
        search
            .search(SearchRequest {
                query: "before maintenance".into(),
                scope: SearchScope::Curated,
                result_type: ResultType::All,
                origin: SearchOrigin::Human,
                limit: 5,
            })
            .unwrap();

        let engine = MaintenanceEngine::start(
            path.clone(),
            BackupStore::open(root.join("backups")).unwrap(),
            Vec::new(),
        )
        .unwrap();
        engine.request(MaintenanceRequest::Compact).unwrap();

        let outcome = search
            .search(SearchRequest {
                query: "during maintenance".into(),
                scope: SearchScope::Curated,
                result_type: ResultType::All,
                origin: SearchOrigin::Human,
                limit: 5,
            })
            .unwrap();
        assert!(matches!(
            outcome.warnings.as_slice(),
            [SearchWarning::HistoryNotRecorded { technical_detail }]
                if technical_detail.contains("MAINTENANCE_IN_PROGRESS")
        ));
        let failure = search.clear_history().unwrap_err();
        assert_eq!(failure.kind, FailureKind::Maintenance);
        assert_eq!(search.history(10).unwrap().len(), 1);
        assert_eq!(search.history(10).unwrap()[0].query, "before maintenance");

        drop(db);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = engine.snapshot().unwrap().unwrap();
            if snapshot.status != MaintenanceStatus::Running {
                assert_eq!(snapshot.status, MaintenanceStatus::Succeeded);
                break;
            }
            assert!(Instant::now() < deadline, "maintenance did not finish");
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn short_terms_filter_candidates_instead_of_being_dropped() {
        let db = memory_db();
        for id in 1..=1_100i64 {
            let url = format!("https://short-term.test/{id}");
            db.conn
                .execute(
                    "INSERT INTO resources(id,url,canonical_url,kind,title,purpose_zh,status,curation_state,health,source,created_at,updated_at)
                     VALUES(?1,?2,?2,'page',?3,?4,'active','active','healthy','gui',?1,?1)",
                    params![
                        id,
                        url,
                        format!("Shared marker {id}"),
                        if id == 97 {
                            "short term regression marker qz".to_owned()
                        } else {
                            format!("short term regression marker {id}")
                        }
                    ],
                )
                .unwrap();
        }
        let outcome = LibrarySearch::new(&db)
            .search(SearchRequest {
                query: "short marker qz".into(),
                scope: SearchScope::Curated,
                result_type: ResultType::All,
                origin: SearchOrigin::Agent,
                limit: 5,
            })
            .unwrap();
        assert!(
            outcome
                .results
                .iter()
                .any(|result| result.primary == PrimaryIdentity::Resource(97))
        );
    }

    #[test]
    fn empty_query_and_limit_are_typed_input_failures() {
        let db = memory_db();
        let search = LibrarySearch::new(&db);
        for request in [
            SearchRequest {
                query: " ".into(),
                scope: SearchScope::Curated,
                result_type: ResultType::All,
                origin: SearchOrigin::Human,
                limit: 5,
            },
            SearchRequest {
                query: "x".into(),
                scope: SearchScope::Curated,
                result_type: ResultType::All,
                origin: SearchOrigin::Human,
                limit: 201,
            },
        ] {
            assert_eq!(search.search(request).unwrap_err().kind, FailureKind::Input);
        }
    }

    #[test]
    fn expired_budget_interrupts_search() {
        let db = memory_db();
        let search = LibrarySearch::with_budget(&db, Duration::ZERO);
        let failure = search
            .search(SearchRequest {
                query: "anything".into(),
                scope: SearchScope::Curated,
                result_type: ResultType::All,
                origin: SearchOrigin::Agent,
                limit: 5,
            })
            .unwrap_err();
        assert_eq!(failure.kind, FailureKind::Storage);
    }

    #[test]
    fn private_resources_are_local_for_humans_and_rejected_for_agents() {
        let db = memory_db();
        let lifecycle = ResourceLibraryLifecycle::new(&db, &NoHandoff, &FixedClock(10));
        lifecycle
            .apply(
                ResourceLifecycleChange::Create(CreateResource {
                    url: "https://private.test/tool".into(),
                    parent_resource_id: None,
                    linked_article_id: None,
                    kind: ResourceKind::Page,
                    title: Some("Private Atlas".into()),
                    private_note: Some("secret research compass".into()),
                    privacy: ResourcePrivacy::Private,
                    source: ResourceSource::Gui,
                    manual_rating: None,
                }),
                ProjectionScope::Resource(1),
            )
            .unwrap();
        let request = |origin| SearchRequest {
            query: "secret research compass".into(),
            scope: SearchScope::Curated,
            result_type: ResultType::Resource,
            origin,
            limit: 5,
        };
        assert_eq!(
            LibrarySearch::new(&db)
                .search(request(SearchOrigin::Human))
                .unwrap()
                .results
                .len(),
            1
        );
        assert!(
            LibrarySearch::new(&db)
                .search(request(SearchOrigin::Agent))
                .unwrap()
                .results
                .is_empty()
        );
    }

    #[test]
    fn broken_resource_remains_searchable_with_a_bounded_penalty() {
        let db = memory_db();
        for (id, url, health) in [
            (1, "https://healthy.test", "healthy"),
            (2, "https://broken.test", "broken"),
        ] {
            db.conn
                .execute(
                    "INSERT INTO resources(id,url,canonical_url,kind,title,privacy,status,curation_state,health,source,created_at,updated_at)
                     VALUES(?1,?2,?2,'page','shared ranking marker','public','active','active',?3,'gui',1,1)",
                    params![id, url, health],
                )
                .unwrap();
        }
        let results = LibrarySearch::new(&db)
            .search(SearchRequest {
                query: "shared ranking marker".into(),
                scope: SearchScope::Curated,
                result_type: ResultType::Resource,
                origin: SearchOrigin::Human,
                limit: 5,
            })
            .unwrap()
            .results;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].health.as_deref(), Some("healthy"));
        assert!(results[1].factors.contains(&ScoreFactor::BrokenPenalty));
    }

    #[test]
    fn archive_scope_and_retained_notes_follow_the_confirmed_corpus_rules() {
        let db = memory_db();
        let feed_id = db.add_feed("https://archive.test/feed", 1).unwrap();
        db.conn
            .execute(
                "INSERT INTO articles(feed_id,entry_id,url,title,content,starred,archived,fetched_at)
                 VALUES(?1,'archived','https://archive.test/post','Archived Post','archived primary marker',1,1,2)",
                [feed_id],
            )
            .unwrap();
        let article_id = db.conn.last_insert_rowid();
        db.add_favorite_selection(article_id, "retained note marker", None, None, 3)
            .unwrap();
        let search = LibrarySearch::new(&db);
        let request = |query: &str, scope| SearchRequest {
            query: query.into(),
            scope,
            result_type: ResultType::Article,
            origin: SearchOrigin::Human,
            limit: 5,
        };
        assert!(
            search
                .search(request("archived primary marker", SearchScope::Curated))
                .unwrap()
                .results
                .is_empty()
        );
        assert_eq!(
            search
                .search(request("archived primary marker", SearchScope::Archive))
                .unwrap()
                .results[0]
                .primary,
            PrimaryIdentity::Article(article_id)
        );
        assert_eq!(
            search
                .search(request("retained note marker", SearchScope::Curated))
                .unwrap()
                .results[0]
                .primary,
            PrimaryIdentity::Article(article_id)
        );
    }

    #[test]
    fn resource_lifecycle_writes_keep_the_derived_index_consistent() {
        let db = memory_db();
        let lifecycle = ResourceLibraryLifecycle::new(&db, &NoHandoff, &FixedClock(10));
        let created = lifecycle
            .apply(
                ResourceLifecycleChange::Create(CreateResource {
                    url: "https://index.test/tool".into(),
                    parent_resource_id: None,
                    linked_article_id: None,
                    kind: ResourceKind::Page,
                    title: Some("Initial Marker".into()),
                    private_note: None,
                    privacy: ResourcePrivacy::Public,
                    source: ResourceSource::Gui,
                    manual_rating: None,
                }),
                ProjectionScope::Resource(1),
            )
            .unwrap();
        let resource_id = created.affected_resource_ids[0];
        lifecycle
            .apply(
                ResourceLifecycleChange::CompleteManualEdit(CompleteManualEdit {
                    resource_id,
                    title: Some("Updated Index Tool".into()),
                    purpose_zh: Some("atomic lifecycle marker".into()),
                    use_when_zh: None,
                    private_note: None,
                    privacy: ResourcePrivacy::Public,
                    manual_rating: Some(4),
                    categories: vec![Category::Docs],
                    tags: vec![ResourceTag {
                        name: "trigger-marker".into(),
                        language: TagLanguage::En,
                        source: TagSource::Manual,
                    }],
                }),
                ProjectionScope::Resource(resource_id),
            )
            .unwrap();
        let search = |query: &str| {
            LibrarySearch::new(&db)
                .search(SearchRequest {
                    query: query.into(),
                    scope: SearchScope::Curated,
                    result_type: ResultType::Resource,
                    origin: SearchOrigin::Human,
                    limit: 5,
                })
                .unwrap()
                .results
        };
        assert_eq!(
            search("trigger-marker")[0].primary,
            PrimaryIdentity::Resource(resource_id)
        );

        db.conn
            .execute(
                "INSERT INTO resource_snapshots(resource_id,content_hash,cleaned_content,fetched_at)
                 VALUES(?1,'hash','snapshot lifecycle marker',20)",
                [resource_id],
            )
            .unwrap();
        let snapshot_id = db.conn.last_insert_rowid();
        db.conn
            .execute(
                "UPDATE resources SET latest_snapshot_id=?2,updated_at=20 WHERE id=?1",
                params![resource_id, snapshot_id],
            )
            .unwrap();
        assert_eq!(search("snapshot lifecycle marker").len(), 1);

        lifecycle
            .apply(
                ResourceLifecycleChange::SetCurationState {
                    resource_id,
                    target: ResourceCurationState::Archived,
                },
                ProjectionScope::Resource(resource_id),
            )
            .unwrap();
        lifecycle
            .apply(
                ResourceLifecycleChange::Delete { resource_id },
                ProjectionScope::collection(
                    crate::resource_library_lifecycle::ResourceCollection::Archived,
                ),
            )
            .unwrap();
        assert!(search("snapshot lifecycle marker").is_empty());
        verify_index(&db.conn).unwrap();
    }

    #[test]
    #[ignore = "local performance benchmark; run explicitly before release"]
    fn benchmark_records_p50_p95_under_the_two_second_budget() {
        let db = memory_db();
        let tx = db.conn.unchecked_transaction().unwrap();
        tx.execute(
            "INSERT INTO feeds(id,url,next_fetch) VALUES(1,'https://benchmark.test/feed',0)",
            [],
        )
        .unwrap();
        for id in 1..=1_000i64 {
            let url = format!("https://benchmark.test/resource/{id}");
            tx.execute(
                "INSERT INTO resources(id,url,canonical_url,kind,title,purpose_zh,status,curation_state,health,source,created_at,updated_at)
                 VALUES(?1,?2,?2,'page',?3,?4,'active','active','healthy','gui',?1,?1)",
                params![id, url, format!("Benchmark Resource {id}"), format!("benchmark resource marker {id}")],
            )
            .unwrap();
        }
        for id in 1..=10_000i64 {
            tx.execute(
                "INSERT INTO articles(id,feed_id,entry_id,url,title,content,starred,fetched_at)
                 VALUES(?1,1,?2,?3,?4,?5,1,?1)",
                params![
                    id,
                    format!("entry-{id}"),
                    format!("https://benchmark.test/article/{id}"),
                    format!("Benchmark Article {id}"),
                    format!("benchmark article marker {id}")
                ],
            )
            .unwrap();
        }
        for id in 1..=2_000i64 {
            tx.execute(
                "INSERT INTO article_selections(id,article_id,selected_text,is_favorite,created_at,updated_at)
                 VALUES(?1,?1,?2,1,?1,?1)",
                params![id, format!("benchmark note marker {id}")],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        verify_index(&db.conn).unwrap();

        let mut elapsed = Vec::new();
        let queries = (1..=10)
            .map(|index| format!("benchmark resource marker {}", index * 97))
            .chain((1..=10).map(|index| format!("benchmark article marker {}", index * 997)))
            .chain((1..=10).map(|index| format!("benchmark note marker {}", index * 197)));
        for query in queries {
            let started = Instant::now();
            let outcome = LibrarySearch::new(&db)
                .search(SearchRequest {
                    query: query.clone(),
                    scope: SearchScope::Curated,
                    result_type: ResultType::All,
                    origin: SearchOrigin::Agent,
                    limit: 5,
                })
                .unwrap();
            assert!(!outcome.results.is_empty(), "no result for {query}");
            elapsed.push(started.elapsed());
        }
        elapsed.sort();
        let p50 = elapsed[elapsed.len() / 2];
        let p95 = elapsed[(elapsed.len() * 95 / 100).min(elapsed.len() - 1)];
        println!("Library Search benchmark: P50={p50:?}, P95={p95:?}");
        assert!(elapsed.iter().all(|duration| *duration < DEFAULT_BUDGET));
    }
}
