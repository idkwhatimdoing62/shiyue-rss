//! Independent resource-library domain and persistence service (spec phase 1).
#![allow(dead_code)] // Phase 2-4 consumers intentionally land after this boundary.

use anyhow::{Context, Result, bail};
use reqwest::Url;
use rusqlite::{OptionalExtension, Row, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::db::Db;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Site,
    Page,
    Article,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourcePrivacy {
    Public,
    Private,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceStatus {
    PendingReview,
    EnrichmentPending,
    Active,
    Broken,
    Archived,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceSource {
    Gui,
    CliAgent,
    Import,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pricing {
    Free,
    Freemium,
    Paid,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Tool,
    AssetLibrary,
    Docs,
    Blog,
    Inspiration,
    Service,
    Repository,
    Other,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagSource {
    Manual,
    Ai,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagLanguage {
    Zh,
    En,
    Other,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrichmentStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageEventKind {
    Returned,
    ConfirmedUsed,
}

macro_rules! string_enum {
    ($ty:ty, {$($variant:path => $value:literal),+ $(,)?}) => {
        impl $ty {
            fn as_str(self) -> &'static str { match self { $($variant => $value),+ } }
            fn parse(value: &str) -> Result<Self> { match value { $($value => Ok($variant)),+, _ => bail!("invalid {}: {value}", stringify!($ty)) } }
        }
    };
}
string_enum!(ResourceKind, {ResourceKind::Site=>"site", ResourceKind::Page=>"page", ResourceKind::Article=>"article"});
string_enum!(ResourcePrivacy, {ResourcePrivacy::Public=>"public", ResourcePrivacy::Private=>"private"});
string_enum!(ResourceStatus, {ResourceStatus::PendingReview=>"pending_review", ResourceStatus::EnrichmentPending=>"enrichment_pending", ResourceStatus::Active=>"active", ResourceStatus::Broken=>"broken", ResourceStatus::Archived=>"archived"});
string_enum!(ResourceSource, {ResourceSource::Gui=>"gui", ResourceSource::CliAgent=>"cli_agent", ResourceSource::Import=>"import"});
string_enum!(Pricing, {Pricing::Free=>"free", Pricing::Freemium=>"freemium", Pricing::Paid=>"paid", Pricing::Unknown=>"unknown"});
string_enum!(Category, {Category::Tool=>"tool", Category::AssetLibrary=>"asset-library", Category::Docs=>"docs", Category::Blog=>"blog", Category::Inspiration=>"inspiration", Category::Service=>"service", Category::Repository=>"repository", Category::Other=>"other"});
string_enum!(TagSource, {TagSource::Manual=>"manual", TagSource::Ai=>"ai"});
string_enum!(TagLanguage, {TagLanguage::Zh=>"zh", TagLanguage::En=>"en", TagLanguage::Other=>"other"});
string_enum!(EnrichmentStatus, {EnrichmentStatus::Pending=>"pending", EnrichmentStatus::Running=>"running", EnrichmentStatus::Succeeded=>"succeeded", EnrichmentStatus::Failed=>"failed"});
string_enum!(UsageEventKind, {UsageEventKind::Returned=>"returned", UsageEventKind::ConfirmedUsed=>"confirmed_used"});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub id: i64,
    pub url: String,
    pub canonical_url: String,
    pub parent_resource_id: Option<i64>,
    pub linked_article_id: Option<i64>,
    pub kind: ResourceKind,
    pub title: Option<String>,
    pub purpose_zh: Option<String>,
    pub use_when_zh: Option<String>,
    pub capabilities: Vec<String>,
    pub limitations: Vec<String>,
    pub pricing: Option<Pricing>,
    pub requires_login: Option<bool>,
    pub languages: Vec<String>,
    pub private_note: Option<String>,
    pub privacy: ResourcePrivacy,
    pub status: ResourceStatus,
    pub source: ResourceSource,
    pub manual_rating: Option<i64>,
    pub latest_snapshot_id: Option<i64>,
    pub last_checked_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct NewResource {
    pub url: String,
    pub parent_resource_id: Option<i64>,
    pub linked_article_id: Option<i64>,
    pub kind: ResourceKind,
    pub title: Option<String>,
    pub private_note: Option<String>,
    pub privacy: ResourcePrivacy,
    pub source: ResourceSource,
    pub manual_rating: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct SnapshotInput {
    pub fetched_url: Option<String>,
    pub http_status: Option<i64>,
    pub title: Option<String>,
    pub cleaned_content: Option<String>,
    pub fetch_error: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceTag {
    pub name: String,
    pub language: TagLanguage,
    pub source: TagSource,
}

#[derive(Debug, Clone)]
pub struct ImportCandidate {
    pub article_id: i64,
    pub url: String,
    pub title: Option<String>,
    pub already_imported: bool,
}

pub struct ResourceService<'a> {
    db: &'a Db,
}
impl<'a> ResourceService<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }
    pub fn create(&self, input: &NewResource, now: i64) -> Result<Resource> {
        validate_rating(input.manual_rating)?;
        let canonical = canonicalize_url(&input.url)?;
        self.db.conn.execute(
            "INSERT INTO resources(url, canonical_url, parent_resource_id, linked_article_id, kind, title, private_note, privacy, status, source, manual_rating, created_at, updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12)",
            params![input.url.trim(), canonical, input.parent_resource_id, input.linked_article_id, input.kind.as_str(), input.title, input.private_note, input.privacy.as_str(), initial_status(input.source).as_str(), input.source.as_str(), input.manual_rating, now])
            .context("create resource")?;
        let id = self.db.conn.last_insert_rowid();
        self.refresh_search_index(id)?;
        self.get(id)
    }
    pub fn get(&self, id: i64) -> Result<Resource> {
        self.db
            .conn
            .query_row(
                &format!("SELECT {RESOURCE_COLS} FROM resources WHERE id=?1"),
                [id],
                map_resource,
            )
            .context("resource not found")
    }
    pub fn update_content(
        &self,
        id: i64,
        title: Option<&str>,
        purpose: Option<&str>,
        use_when: Option<&str>,
        capabilities: &[String],
        limitations: &[String],
        pricing: Option<Pricing>,
        requires_login: Option<bool>,
        languages: &[String],
        rating: Option<i64>,
        now: i64,
    ) -> Result<()> {
        validate_rating(rating)?;
        validate_phrases(capabilities)?;
        validate_phrases(limitations)?;
        validate_phrases(languages)?;
        let capabilities = serde_json::to_string(capabilities)?;
        let limitations = serde_json::to_string(limitations)?;
        let languages = serde_json::to_string(languages)?;
        let changed = self.db.conn.execute("UPDATE resources SET title=?2,purpose_zh=?3,use_when_zh=?4,capabilities=?5,limitations=?6,pricing=?7,requires_login=?8,languages=?9,manual_rating=?10,updated_at=?11 WHERE id=?1", params![id,title,purpose,use_when,capabilities,limitations,pricing.map(Pricing::as_str),requires_login,languages,rating,now])?;
        if changed == 0 {
            bail!("resource not found")
        }
        self.refresh_search_index(id)?;
        Ok(())
    }
    pub fn update_manual_fields(
        &self,
        id: i64,
        title: Option<&str>,
        purpose_zh: Option<&str>,
        private_note: Option<&str>,
        privacy: ResourcePrivacy,
        manual_rating: Option<i64>,
        now: i64,
    ) -> Result<()> {
        validate_rating(manual_rating)?;
        let changed = self.db.conn.execute(
            "UPDATE resources SET title=?2,purpose_zh=?3,purpose_source=CASE WHEN ?3 IS NULL THEN purpose_source ELSE 'manual' END,private_note=?4,privacy=?5,manual_rating=?6,updated_at=?7 WHERE id=?1",
            params![id,title,purpose_zh,private_note,privacy.as_str(),manual_rating,now],
        )?;
        if changed == 0 {
            bail!("resource not found")
        }
        self.refresh_search_index(id)?;
        Ok(())
    }
    pub fn set_fetched_title_if_empty(&self, id: i64, title: Option<&str>, now: i64) -> Result<()> {
        self.db.conn.execute(
            "UPDATE resources SET title=COALESCE(title,?2),last_checked_at=?3,updated_at=?3 WHERE id=?1",
            params![id,title,now],
        )?;
        self.refresh_search_index(id)?;
        Ok(())
    }
    pub fn enrichment_input(
        &self,
        id: i64,
    ) -> Result<Option<crate::resource_enrichment::EnrichmentInput>> {
        let resource = self.get(id)?;
        if resource.privacy == ResourcePrivacy::Private {
            return Ok(None);
        }
        let content = if let Some(snapshot_id) = resource.latest_snapshot_id {
            self.db.conn.query_row(
                "SELECT COALESCE(cleaned_content,'') FROM resource_snapshots WHERE id=?1",
                [snapshot_id],
                |r| r.get::<_, String>(0),
            )?
        } else if let Some(article_id) = resource.linked_article_id {
            self.db.conn.query_row(
                "SELECT COALESCE(content,'') FROM articles WHERE id=?1",
                [article_id],
                |r| r.get::<_, String>(0),
            )?
        } else {
            String::new()
        };
        Ok(Some(crate::resource_enrichment::EnrichmentInput {
            resource_id: id,
            url: resource.url,
            title: resource.title,
            private_note: resource.private_note,
            cleaned_content: content,
        }))
    }
    pub fn apply_enrichment(
        &self,
        id: i64,
        output: &crate::resource_enrichment::EnrichmentOutput,
        now: i64,
    ) -> Result<()> {
        let capabilities = serde_json::to_string(&output.capabilities)?;
        let limitations = serde_json::to_string(&output.limitations)?;
        let languages = serde_json::to_string(&output.languages)?;
        let tx = self.db.conn.unchecked_transaction()?;
        tx.execute("UPDATE resources SET purpose_zh=CASE WHEN purpose_source='manual' THEN purpose_zh ELSE ?2 END,purpose_source=CASE WHEN purpose_source='manual' THEN 'manual' ELSE 'ai' END,use_when_zh=CASE WHEN use_when_source='manual' THEN use_when_zh ELSE ?3 END,use_when_source=CASE WHEN use_when_source='manual' THEN 'manual' ELSE 'ai' END,capabilities=?4,limitations=?5,pricing=?6,requires_login=?7,languages=?8,updated_at=?9 WHERE id=?1",params![id,output.purpose_zh,output.use_when_zh,capabilities,limitations,output.pricing,output.requires_login,languages,now])?;
        tx.execute("DELETE FROM resource_categories WHERE resource_id=?1", [id])?;
        for category in &output.categories {
            Category::parse(category)?;
            tx.execute(
                "INSERT INTO resource_categories(resource_id,category) VALUES(?1,?2)",
                params![id, category],
            )?;
        }
        tx.execute(
            "DELETE FROM resource_tags WHERE resource_id=?1 AND source='ai'",
            [id],
        )?;
        for (name, language) in output
            .tags_zh
            .iter()
            .map(|v| (v, "zh"))
            .chain(output.tags_en.iter().map(|v| (v, "en")))
        {
            if name.trim().is_empty() || name.chars().count() > 100 {
                bail!("invalid AI tag")
            };
            tx.execute("INSERT INTO resource_tags(resource_id,name,language,source,created_at) VALUES(?1,?2,?3,'ai',?4) ON CONFLICT(resource_id,name) DO NOTHING",params![id,name,language,now])?;
        }
        tx.commit()?;
        self.refresh_search_index(id)?;
        Ok(())
    }
    pub fn transition(&self, id: i64, to: ResourceStatus, now: i64) -> Result<()> {
        let from = self.get(id)?.status;
        if !valid_transition(from, to) {
            bail!(
                "invalid resource transition: {} -> {}",
                from.as_str(),
                to.as_str()
            );
        }
        self.db.conn.execute(
            "UPDATE resources SET status=?2,updated_at=?3 WHERE id=?1",
            params![id, to.as_str(), now],
        )?;
        Ok(())
    }
    pub fn delete(&self, id: i64) -> Result<()> {
        let status = self.get(id)?.status;
        if !matches!(
            status,
            ResourceStatus::PendingReview | ResourceStatus::Archived
        ) {
            bail!("physical delete is not allowed in this state")
        };
        self.db
            .conn
            .execute("DELETE FROM resources WHERE id=?1", [id])?;
        Ok(())
    }
    pub fn set_categories(&self, id: i64, categories: &[Category]) -> Result<()> {
        let tx = self.db.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM resource_categories WHERE resource_id=?1", [id])?;
        for c in categories {
            tx.execute(
                "INSERT OR IGNORE INTO resource_categories(resource_id,category) VALUES(?1,?2)",
                params![id, c.as_str()],
            )?;
        }
        tx.commit()?;
        self.refresh_search_index(id)?;
        Ok(())
    }
    pub fn upsert_tag(&self, id: i64, tag: &ResourceTag, now: i64) -> Result<()> {
        let name = tag.name.trim();
        if name.is_empty() || name.chars().count() > 100 {
            bail!("invalid tag")
        };
        self.db.conn.execute("INSERT INTO resource_tags(resource_id,name,language,source,created_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(resource_id,name) DO UPDATE SET language=CASE WHEN resource_tags.source='manual' THEN resource_tags.language ELSE excluded.language END, source=CASE WHEN resource_tags.source='manual' THEN 'manual' ELSE excluded.source END",params![id,name,tag.language.as_str(),tag.source.as_str(),now])?;
        self.refresh_search_index(id)?;
        Ok(())
    }
    pub fn tags(&self, id: i64) -> Result<Vec<ResourceTag>> {
        let mut s=self.db.conn.prepare("SELECT name,language,source FROM resource_tags WHERE resource_id=?1 ORDER BY name COLLATE NOCASE")?;
        let rows = s.query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        rows.map(|r| {
            let (n, l, s) = r?;
            Ok(ResourceTag {
                name: n,
                language: TagLanguage::parse(&l)?,
                source: TagSource::parse(&s)?,
            })
        })
        .collect()
    }
    pub fn record_snapshot(&self, id: i64, input: &SnapshotInput, now: i64) -> Result<Option<i64>> {
        if input.fetch_error.is_some() {
            self.db.conn.execute("INSERT INTO resource_snapshots(resource_id,fetched_url,http_status,title,fetched_at,fetch_error) VALUES(?1,?2,?3,?4,?5,?6)",params![id,input.fetched_url,input.http_status,input.title,now,input.fetch_error])?;
            self.db.conn.execute(
                "UPDATE resources SET last_checked_at=?2,updated_at=?2 WHERE id=?1",
                params![id, now],
            )?;
            return Ok(Some(self.db.conn.last_insert_rowid()));
        }
        let hash = snapshot_hash(input);
        let existing: Option<i64> = self
            .db
            .conn
            .query_row(
                "SELECT id FROM resource_snapshots WHERE resource_id=?1 AND content_hash=?2",
                params![id, hash],
                |r| r.get(0),
            )
            .optional()?;
        if existing.is_some() {
            self.db.conn.execute(
                "UPDATE resources SET last_checked_at=?2,updated_at=?2 WHERE id=?1",
                params![id, now],
            )?;
            return Ok(None);
        }
        self.db.conn.execute("INSERT INTO resource_snapshots(resource_id,content_hash,fetched_url,http_status,title,cleaned_content,fetched_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![id,hash,input.fetched_url,input.http_status,input.title,input.cleaned_content,now])?;
        let sid = self.db.conn.last_insert_rowid();
        self.db.conn.execute("UPDATE resources SET latest_snapshot_id=?2,last_checked_at=?3,updated_at=?3 WHERE id=?1",params![id,sid,now])?;
        self.refresh_search_index(id)?;
        Ok(Some(sid))
    }
    pub fn start_enrichment(
        &self,
        id: i64,
        snapshot_id: Option<i64>,
        provider: &str,
        model: &str,
        prompt: &str,
        schema: &str,
        now: i64,
    ) -> Result<i64> {
        self.db.conn.execute("INSERT INTO resource_enrichment_runs(resource_id,snapshot_id,provider,model,prompt_version,schema_version,started_at,status) VALUES(?1,?2,?3,?4,?5,?6,?7,'pending')",params![id,snapshot_id,provider,model,prompt,schema,now])?;
        Ok(self.db.conn.last_insert_rowid())
    }
    pub fn finish_enrichment(
        &self,
        run: i64,
        status: EnrichmentStatus,
        error_code: Option<&str>,
        error_message: Option<&str>,
        now: i64,
    ) -> Result<()> {
        self.db.conn.execute("UPDATE resource_enrichment_runs SET status=?2,finished_at=?3,error_code=?4,error_message=?5 WHERE id=?1",params![run,status.as_str(),now,error_code,error_message])?;
        Ok(())
    }
    pub fn record_usage(&self, id: i64, event: UsageEventKind, now: i64) -> Result<()> {
        self.db.conn.execute(
            "INSERT INTO resource_usage_events(resource_id,event,occurred_at) VALUES(?1,?2,?3)",
            params![id, event.as_str(), now],
        )?;
        Ok(())
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<Resource>> {
        self.list_where("1=1", limit)
    }

    pub fn pending(&self) -> Result<Vec<Resource>> {
        self.list_where("status IN ('pending_review','enrichment_pending')", 1000)
    }

    pub fn preview_web_clipping_import(&self) -> Result<Vec<ImportCandidate>> {
        let mut candidates = Vec::new();
        for article in self.db.web_clippings()? {
            let Some(url) = article.url else { continue };
            let canonical = canonicalize_url(&url)?;
            let existing:i64=self.db.conn.query_row("SELECT COUNT(*) FROM resources WHERE canonical_url=?1 OR (source='import' AND linked_article_id=?2)",params![canonical,article.id],|r|r.get(0))?;
            candidates.push(ImportCandidate {
                article_id: article.id,
                url,
                title: article.title,
                already_imported: existing > 0,
            });
        }
        Ok(candidates)
    }
    pub fn import_web_clippings(&self, article_ids: &[i64], now: i64) -> Result<Vec<i64>> {
        let tx = self.db.conn.unchecked_transaction()?;
        let mut ids = Vec::new();
        for article_id in article_ids {
            let row:Option<(String,Option<String>)>=tx.query_row("SELECT a.url,a.title FROM articles a JOIN feeds f ON f.id=a.feed_id WHERE a.id=?1 AND f.url=?2 AND a.url IS NOT NULL",params![article_id,crate::db::WEB_CLIPPINGS_FEED_URL],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let Some((url, title)) = row else { continue };
            let canonical = canonicalize_url(&url)?;
            tx.execute("INSERT OR IGNORE INTO resources(url,canonical_url,linked_article_id,kind,title,privacy,status,source,created_at,updated_at) VALUES(?1,?2,?3,'article',?4,'public','active','import',?5,?5)",params![url,canonical,article_id,title,now])?;
            if tx.changes() > 0 {
                ids.push(tx.last_insert_rowid());
            }
        }
        tx.commit()?;
        for id in &ids {
            self.refresh_search_index(*id)?;
        }
        Ok(ids)
    }

    fn list_where(&self, predicate: &str, limit: usize) -> Result<Vec<Resource>> {
        let sql = format!(
            "SELECT {RESOURCE_COLS} FROM resources WHERE {predicate} ORDER BY updated_at DESC,id DESC LIMIT ?1"
        );
        let mut stmt = self.db.conn.prepare(&sql)?;
        let rows = stmt.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], map_resource)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn refresh_search_index(&self, id: i64) -> Result<()> {
        self.db
            .conn
            .execute("DELETE FROM resource_fts WHERE source_id=?1", [id])?;
        self.db.conn.execute("INSERT INTO resource_fts(source_id,body) SELECT r.id,trim(COALESCE(r.title,'')||char(10)||r.url||char(10)||COALESCE(r.purpose_zh,'')||char(10)||COALESCE(r.use_when_zh,'')||char(10)||r.capabilities||char(10)||r.limitations||char(10)||r.languages||char(10)||COALESCE(r.private_note,'')||char(10)||COALESCE((SELECT group_concat(category,' ') FROM resource_categories c WHERE c.resource_id=r.id),'')||char(10)||COALESCE((SELECT group_concat(name,' ') FROM resource_tags t WHERE t.resource_id=r.id),'')||char(10)||COALESCE((SELECT cleaned_content FROM resource_snapshots s WHERE s.id=r.latest_snapshot_id),'')) FROM resources r WHERE r.id=?1", [id])?;
        Ok(())
    }

    pub fn search_json(
        &self,
        query: &str,
        include_resources: bool,
        include_articles: bool,
        all_articles: bool,
        limit: usize,
    ) -> Result<Vec<Value>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let pattern = format!(
            "%{}%",
            query.to_lowercase().replace('%', "\\%").replace('_', "\\_")
        );
        let mut results = Vec::new();
        if include_resources {
            let fts_query = if query
                .split_whitespace()
                .all(|term| term.chars().count() >= 3)
            {
                query
                    .split_whitespace()
                    .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
                    .collect::<Vec<_>>()
                    .join(" AND ")
            } else {
                String::new()
            };
            let fallback_terms = query
                .split_whitespace()
                .map(str::to_lowercase)
                .collect::<Vec<_>>();
            let sql_pattern = if fts_query.is_empty() {
                "%".to_owned()
            } else {
                pattern.clone()
            };
            let sql_limit = if fts_query.is_empty() { 1000 } else { limit };
            let mut stmt = self.db.conn.prepare(&format!(
                "SELECT {RESOURCE_COLS},
                    CASE WHEN lower(COALESCE(title,'')) LIKE ?1 ESCAPE '\\' THEN 'title'
                         WHEN lower(url) LIKE ?1 ESCAPE '\\' THEN 'url'
                         WHEN lower(COALESCE(purpose_zh,'')) LIKE ?1 ESCAPE '\\' THEN 'purpose_zh'
                         WHEN lower(COALESCE(use_when_zh,'')) LIKE ?1 ESCAPE '\\' THEN 'use_when_zh'
                         WHEN lower(private_note) LIKE ?1 ESCAPE '\\' THEN 'private_note'
                         WHEN lower(capabilities || limitations || languages || COALESCE((SELECT group_concat(category,' ') FROM resource_categories c WHERE c.resource_id=resources.id),'') || COALESCE((SELECT group_concat(name,' ') FROM resource_tags t WHERE t.resource_id=resources.id),'')) LIKE ?1 ESCAPE '\\' THEN 'metadata'
                         ELSE 'snapshot' END matched_field,
                    COALESCE((SELECT cleaned_content FROM resource_snapshots s WHERE s.id=resources.latest_snapshot_id),'') snapshot
                 FROM resources WHERE status='active' AND ((?3='' AND lower(COALESCE(title,'')||char(10)||url||char(10)||COALESCE(purpose_zh,'')||char(10)||COALESCE(use_when_zh,'')||char(10)||capabilities||char(10)||limitations||char(10)||languages||char(10)||COALESCE(private_note,'')||char(10)||COALESCE((SELECT group_concat(category,' ') FROM resource_categories c WHERE c.resource_id=resources.id),'')||char(10)||COALESCE((SELECT group_concat(name,' ') FROM resource_tags t WHERE t.resource_id=resources.id),'')||char(10)||COALESCE((SELECT cleaned_content FROM resource_snapshots s WHERE s.id=resources.latest_snapshot_id),'')) LIKE ?1 ESCAPE '\\') OR (?3<>'' AND resources.id IN (SELECT source_id FROM resource_fts WHERE resource_fts MATCH ?3)))
                 ORDER BY COALESCE(manual_rating,0) DESC,updated_at DESC LIMIT ?2"))?;
            let rows = stmt.query_map(
                params![
                    sql_pattern,
                    i64::try_from(sql_limit).unwrap_or(i64::MAX),
                    fts_query
                ],
                |row| {
                    let resource = map_resource(row)?;
                    let field: String = row.get(23)?;
                    let snapshot: String = row.get(24)?;
                    Ok((resource, field, snapshot))
                },
            )?;
            for row in rows {
                let (r, mut field, snapshot) = row?;
                let haystack = format!(
                    "{} {} {} {} {} {} {} {}",
                    r.title.as_deref().unwrap_or(""),
                    r.url,
                    r.purpose_zh.as_deref().unwrap_or(""),
                    r.use_when_zh.as_deref().unwrap_or(""),
                    r.capabilities.join(" "),
                    r.limitations.join(" "),
                    r.languages.join(" "),
                    snapshot
                );
                if !fallback_terms.is_empty()
                    && !fallback_terms
                        .iter()
                        .all(|term| haystack.to_lowercase().contains(term))
                {
                    continue;
                }
                if !fallback_terms.is_empty() {
                    field = "multi_field".into();
                }
                results.push(self.to_json(
                    &r,
                    &field,
                    evidence_snippet(&haystack, query),
                    1.0 + r.manual_rating.unwrap_or(0) as f64 * 0.05,
                )?);
            }
        }
        if include_articles {
            let scope = if all_articles {
                "a.archived=0 AND ?3 IS NOT NULL"
            } else {
                "a.archived=0 AND (a.starred=1 OR f.url=?3)"
            };
            let sql = format!(
                "SELECT a.id,a.url,a.title,a.content,a.starred,a.fetched_at,f.url FROM articles a JOIN feeds f ON f.id=a.feed_id WHERE {scope} AND lower(COALESCE(a.title,'')||char(10)||COALESCE(a.content,'')||char(10)||COALESCE(a.url,'')) LIKE ?1 ESCAPE '\\' ORDER BY COALESCE(a.published,a.fetched_at) DESC LIMIT ?2"
            );
            let mut stmt = self.db.conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params![
                    pattern,
                    i64::try_from(limit).unwrap_or(i64::MAX),
                    crate::db::WEB_CLIPPINGS_FEED_URL
                ],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, bool>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, String>(6)?,
                    ))
                },
            )?;
            for row in rows {
                let (id, url, title, content, starred, updated, feed) = row?;
                results.push(json!({"id":id.to_string(),"result_type":"article","url":url,"title":title,"kind":if feed==crate::db::WEB_CLIPPINGS_FEED_URL{"web_clipping"}else{"article"},"categories":[],"tags":[],"purpose_zh":null,"use_when_zh":null,"capabilities":[],"limitations":[],"pricing":null,"requires_login":null,"private_note":null,"matched_fields":["article_text"],"evidence_snippets":[{"source_type":"article","article_id":id.to_string(),"snapshot_id":null,"text":evidence_snippet(content.as_deref().unwrap_or(""),query)}],"updated_at":rfc3339(updated),"last_checked_at":null,"status":"active","score":1.0,"score_factors":[if starred{"starred"}else{"web_clipping"}]}));
            }
        }
        results.truncate(limit);
        Ok(results)
    }

    pub fn to_json(
        &self,
        resource: &Resource,
        matched_field: &str,
        snippet: String,
        score: f64,
    ) -> Result<Value> {
        let mut value = resource_json(resource, matched_field, snippet, score)?;
        let categories = {
            let mut stmt = self.db.conn.prepare(
                "SELECT category FROM resource_categories WHERE resource_id=?1 ORDER BY category",
            )?;
            stmt.query_map([resource.id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let tags = self
            .tags(resource.id)?
            .into_iter()
            .map(|tag| {
                json!({
                    "name": tag.name,
                    "language": tag.language.as_str(),
                    "source": tag.source.as_str()
                })
            })
            .collect::<Vec<_>>();
        value["categories"] = json!(categories);
        value["tags"] = json!(tags);
        Ok(value)
    }
}

pub(crate) fn resource_json(
    r: &Resource,
    field: &str,
    snippet: String,
    score: f64,
) -> Result<Value> {
    Ok(
        json!({"id":r.id.to_string(),"result_type":"resource","url":r.url,"title":r.title,"kind":r.kind.as_str(),"categories":[],"tags":[],"purpose_zh":r.purpose_zh,"use_when_zh":r.use_when_zh,"capabilities":r.capabilities,"limitations":r.limitations,"pricing":r.pricing.map(Pricing::as_str),"requires_login":r.requires_login,"private_note":r.private_note.as_ref().map(|v|json!({"value":v,"source":"local_private_note"})),"matched_fields":[field],"evidence_snippets":[{"source_type":field,"snapshot_id":r.latest_snapshot_id.map(|v|v.to_string()),"article_id":r.linked_article_id.map(|v|v.to_string()),"text":snippet}],"updated_at":rfc3339(r.updated_at),"last_checked_at":r.last_checked_at.map(rfc3339),"status":r.status.as_str(),"score":score,"score_factors":["text_match",if r.manual_rating.is_some(){"manual_rating_boost"}else{"no_rating_boost"}]}),
    )
}
fn rfc3339(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
fn evidence_snippet(text: &str, query: &str) -> String {
    let clean = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = clean.to_lowercase();
    let q = query.to_lowercase();
    if let Some(byte) = lower.find(&q) {
        let start = clean[..byte]
            .char_indices()
            .rev()
            .nth(40)
            .map_or(0, |(i, _)| i);
        let end = clean[byte..]
            .char_indices()
            .nth(q.chars().count() + 80)
            .map_or(clean.len(), |(i, _)| byte + i);
        clean[start..end].to_string()
    } else {
        clean.chars().take(160).collect()
    }
}

const RESOURCE_COLS: &str = "id,url,canonical_url,parent_resource_id,linked_article_id,kind,title,purpose_zh,use_when_zh,capabilities,limitations,pricing,requires_login,languages,private_note,privacy,status,source,manual_rating,latest_snapshot_id,last_checked_at,created_at,updated_at";
fn map_resource(r: &Row) -> rusqlite::Result<Resource> {
    fn conv(e: anyhow::Error) -> rusqlite::Error {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.into())
    }
    let kind = ResourceKind::parse(&r.get::<_, String>(5)?).map_err(conv)?;
    let pricing = r
        .get::<_, Option<String>>(11)?
        .map(|v| Pricing::parse(&v))
        .transpose()
        .map_err(conv)?;
    Ok(Resource {
        id: r.get(0)?,
        url: r.get(1)?,
        canonical_url: r.get(2)?,
        parent_resource_id: r.get(3)?,
        linked_article_id: r.get(4)?,
        kind,
        title: r.get(6)?,
        purpose_zh: r.get(7)?,
        use_when_zh: r.get(8)?,
        capabilities: serde_json::from_str(&r.get::<_, String>(9)?).map_err(|e| conv(e.into()))?,
        limitations: serde_json::from_str(&r.get::<_, String>(10)?).map_err(|e| conv(e.into()))?,
        pricing,
        requires_login: r.get(12)?,
        languages: serde_json::from_str(&r.get::<_, String>(13)?).map_err(|e| conv(e.into()))?,
        private_note: r.get(14)?,
        privacy: ResourcePrivacy::parse(&r.get::<_, String>(15)?).map_err(conv)?,
        status: ResourceStatus::parse(&r.get::<_, String>(16)?).map_err(conv)?,
        source: ResourceSource::parse(&r.get::<_, String>(17)?).map_err(conv)?,
        manual_rating: r.get(18)?,
        latest_snapshot_id: r.get(19)?,
        last_checked_at: r.get(20)?,
        created_at: r.get(21)?,
        updated_at: r.get(22)?,
    })
}
fn initial_status(source: ResourceSource) -> ResourceStatus {
    match source {
        ResourceSource::CliAgent => ResourceStatus::PendingReview,
        _ => ResourceStatus::EnrichmentPending,
    }
}
fn valid_transition(from: ResourceStatus, to: ResourceStatus) -> bool {
    matches!(
        (from, to),
        (
            ResourceStatus::PendingReview,
            ResourceStatus::Active | ResourceStatus::Archived
        ) | (
            ResourceStatus::EnrichmentPending,
            ResourceStatus::Active | ResourceStatus::EnrichmentPending
        ) | (
            ResourceStatus::Active,
            ResourceStatus::Broken | ResourceStatus::Archived
        ) | (
            ResourceStatus::Broken,
            ResourceStatus::Active | ResourceStatus::Archived
        ) | (ResourceStatus::Archived, ResourceStatus::Active)
    )
}
fn validate_rating(v: Option<i64>) -> Result<()> {
    if v.is_some_and(|n| !(1..=5).contains(&n)) {
        bail!("manual rating must be 1..=5")
    }
    Ok(())
}
fn validate_phrases(v: &[String]) -> Result<()> {
    if v.iter()
        .any(|s| s.trim().is_empty() || s.chars().count() > 500)
    {
        bail!("invalid string array item")
    }
    Ok(())
}
fn canonicalize_url(raw: &str) -> Result<String> {
    let mut u = Url::parse(raw.trim()).context("invalid resource URL")?;
    if !matches!(u.scheme(), "http" | "https") {
        bail!("resource URL must use HTTP(S)")
    }
    u.set_fragment(None);
    let remove_port = (u.scheme() == "http" && u.port() == Some(80))
        || (u.scheme() == "https" && u.port() == Some(443));
    if remove_port {
        u.set_port(None)
            .map_err(|_| anyhow::anyhow!("invalid port"))?;
    }
    if u.path().is_empty() {
        u.set_path("/")
    }
    Ok(u.to_string())
}
fn snapshot_hash(i: &SnapshotInput) -> String {
    let mut h = Sha256::new();
    h.update(i.title.as_deref().unwrap_or("").as_bytes());
    h.update([0]);
    h.update(i.cleaned_content.as_deref().unwrap_or("").as_bytes());
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    fn service_db() -> Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(super::super::db::SCHEMA).unwrap();
        Db { conn, path: None }
    }
    fn input(url: &str, source: ResourceSource) -> NewResource {
        NewResource {
            url: url.into(),
            parent_resource_id: None,
            linked_article_id: None,
            kind: ResourceKind::Page,
            title: None,
            private_note: None,
            privacy: ResourcePrivacy::Public,
            source,
            manual_rating: None,
        }
    }
    #[test]
    fn canonical_dedup_keeps_different_pages() {
        let db = service_db();
        let s = ResourceService::new(&db);
        s.create(
            &input("https://EXAMPLE.com:443/a#x", ResourceSource::Gui),
            1,
        )
        .unwrap();
        assert!(
            s.create(&input("https://example.com/a", ResourceSource::Gui), 2)
                .is_err()
        );
        assert!(
            s.create(&input("https://example.com/b", ResourceSource::Gui), 2)
                .is_ok()
        );
    }
    #[test]
    fn initial_state_and_transitions_are_enforced() {
        let db = service_db();
        let s = ResourceService::new(&db);
        let r = s
            .create(&input("https://example.com", ResourceSource::CliAgent), 1)
            .unwrap();
        assert_eq!(r.status, ResourceStatus::PendingReview);
        assert!(s.transition(r.id, ResourceStatus::Broken, 2).is_err());
        s.transition(r.id, ResourceStatus::Active, 2).unwrap();
        s.transition(r.id, ResourceStatus::Archived, 3).unwrap();
        s.transition(r.id, ResourceStatus::Active, 4).unwrap();
    }
    #[test]
    fn manual_tag_wins_and_snapshot_is_content_addressed() {
        let db = service_db();
        let s = ResourceService::new(&db);
        let r = s
            .create(&input("https://example.com", ResourceSource::Gui), 1)
            .unwrap();
        s.upsert_tag(
            r.id,
            &ResourceTag {
                name: "rust".into(),
                language: TagLanguage::En,
                source: TagSource::Manual,
            },
            1,
        )
        .unwrap();
        s.upsert_tag(
            r.id,
            &ResourceTag {
                name: "RUST".into(),
                language: TagLanguage::Zh,
                source: TagSource::Ai,
            },
            2,
        )
        .unwrap();
        assert_eq!(s.tags(r.id).unwrap()[0].source, TagSource::Manual);
        let snap = SnapshotInput {
            fetched_url: Some(r.url),
            http_status: Some(200),
            title: Some("A".into()),
            cleaned_content: Some("body".into()),
            fetch_error: None,
        };
        assert!(s.record_snapshot(r.id, &snap, 3).unwrap().is_some());
        assert_eq!(s.record_snapshot(r.id, &snap, 4).unwrap(), None);
    }
    #[test]
    fn deleting_resource_preserves_linked_article() {
        let db = service_db();
        let feed = db.add_feed("https://x/feed", 0).unwrap();
        db.conn
            .execute(
                "INSERT INTO articles(feed_id,entry_id,fetched_at) VALUES(?1,'a',0)",
                [feed],
            )
            .unwrap();
        let aid = db.conn.last_insert_rowid();
        let mut i = input("https://x/a", ResourceSource::CliAgent);
        i.linked_article_id = Some(aid);
        let r = ResourceService::new(&db).create(&i, 1).unwrap();
        ResourceService::new(&db).delete(r.id).unwrap();
        let n: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM articles WHERE id=?1", [aid], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn legacy_schema_upgrades_idempotently_without_losing_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(
            "CREATE TABLE feeds(id INTEGER PRIMARY KEY,url TEXT NOT NULL UNIQUE,title TEXT,interval_secs INTEGER,last_fetch INTEGER,next_fetch INTEGER NOT NULL DEFAULT 0,last_error TEXT,fail_count INTEGER NOT NULL DEFAULT 0,disabled INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE articles(id INTEGER PRIMARY KEY,feed_id INTEGER NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,entry_id TEXT NOT NULL,url TEXT,title TEXT,author TEXT,published INTEGER,content TEXT,is_read INTEGER NOT NULL DEFAULT 0,starred INTEGER NOT NULL DEFAULT 0,read_later INTEGER NOT NULL DEFAULT 0,archived INTEGER NOT NULL DEFAULT 0,fetched_at INTEGER NOT NULL,UNIQUE(feed_id,entry_id));
             INSERT INTO feeds(id,url) VALUES(1,'https://legacy.test/feed');
             INSERT INTO articles(id,feed_id,entry_id,title,fetched_at) VALUES(1,1,'entry','legacy',1);",
        ).unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        let legacy: String = conn
            .query_row("SELECT title FROM articles WHERE id=1", [], |r| r.get(0))
            .unwrap();
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(legacy, "legacy");
        assert_eq!(integrity, "ok");
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM resources", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn supplied_database_copy_upgrades_and_passes_integrity_check() {
        let Ok(source) = std::env::var("RRSS_MIGRATION_SOURCE") else {
            return;
        };
        let destination = std::env::temp_dir().join(format!(
            "rrss-phase1-migration-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::copy(&source, &destination).unwrap();
        let db = Db::open(&destination).unwrap();
        let check = db.integrity_check().unwrap();
        assert!(check.ok, "{}", check.details);
        drop(db);
        std::fs::remove_file(destination).unwrap();
    }

    #[test]
    fn validates_fields_and_persists_phase_one_auxiliary_records() {
        let db = service_db();
        let service = ResourceService::new(&db);
        let mut bad = input("https://example.com", ResourceSource::Gui);
        bad.manual_rating = Some(6);
        assert!(service.create(&bad, 1).is_err());
        let resource = service
            .create(&input("https://example.com", ResourceSource::Gui), 1)
            .unwrap();
        service
            .set_categories(resource.id, &[Category::Tool, Category::Docs])
            .unwrap();
        service
            .record_usage(resource.id, UsageEventKind::Returned, 2)
            .unwrap();
        let run = service
            .start_enrichment(
                resource.id,
                None,
                "openai-compatible",
                "fake",
                "resource-v1",
                "1",
                2,
            )
            .unwrap();
        service
            .finish_enrichment(
                run,
                EnrichmentStatus::Failed,
                Some("NO_CREDENTIAL"),
                Some("credential unavailable"),
                3,
            )
            .unwrap();
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM resource_categories WHERE resource_id=?1",
                    [resource.id],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            2
        );
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM resource_usage_events WHERE resource_id=?1",
                    [resource.id],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT status FROM resource_enrichment_runs WHERE id=?1",
                    [run],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "failed"
        );
    }

    #[test]
    fn deterministic_search_excludes_pending_and_uses_bounded_rating_boost() {
        let db = service_db();
        let service = ResourceService::new(&db);
        let first = service
            .create(&input("https://a.test/svg", ResourceSource::Gui), 1)
            .unwrap();
        let pending = service
            .create(&input("https://b.test/svg", ResourceSource::CliAgent), 1)
            .unwrap();
        service
            .update_content(
                first.id,
                Some("SVG editor"),
                Some("制作图标"),
                None,
                &["vector".into()],
                &[],
                None,
                Some(false),
                &[],
                Some(5),
                2,
            )
            .unwrap();
        service.set_categories(first.id, &[Category::Tool]).unwrap();
        service
            .upsert_tag(
                first.id,
                &ResourceTag {
                    name: "图标".into(),
                    language: TagLanguage::Zh,
                    source: TagSource::Manual,
                },
                2,
            )
            .unwrap();
        service
            .transition(first.id, ResourceStatus::Active, 3)
            .unwrap();
        let results = service.search_json("SVG", true, false, false, 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["id"], first.id.to_string());
        assert_ne!(results[0]["id"], pending.id.to_string());
        assert_eq!(results[0]["score_factors"][1], "manual_rating_boost");
        assert_eq!(results[0]["categories"][0], "tool");
        assert_eq!(results[0]["tags"][0]["name"], "图标");
        assert!(
            service
                .search_json("not-present", true, false, false, 5)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn article_search_distinguishes_curated_from_all_scope() {
        let db = service_db();
        let normal = db.add_feed("https://example.com/feed", 0).unwrap();
        let clipping = db.ensure_web_clippings_feed(0).unwrap();
        db.conn.execute("INSERT INTO articles(feed_id,entry_id,title,content,starred,fetched_at) VALUES(?1,'plain','Rust plain','Rust GUI',0,1),(?1,'star','Rust starred','Rust GUI',1,2),(?2,'clip','Rust clipping','Rust GUI',0,3)", params![normal,clipping]).unwrap();
        let service = ResourceService::new(&db);
        let curated = service.search_json("Rust", false, true, false, 10).unwrap();
        let all = service.search_json("Rust", false, true, true, 10).unwrap();
        assert_eq!(curated.len(), 2);
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn private_resources_never_produce_provider_input() {
        let db = service_db();
        let service = ResourceService::new(&db);
        let mut private = input("https://private.test", ResourceSource::Gui);
        private.privacy = ResourcePrivacy::Private;
        let resource = service.create(&private, 1).unwrap();
        assert!(service.enrichment_input(resource.id).unwrap().is_none());
    }

    #[test]
    fn enrichment_preserves_manual_purpose_and_tags() {
        use crate::resource_enrichment::{EnrichmentOutput, Evidence};
        let db = service_db();
        let service = ResourceService::new(&db);
        let resource = service
            .create(&input("https://icons.test", ResourceSource::Gui), 1)
            .unwrap();
        service
            .update_manual_fields(
                resource.id,
                None,
                Some("我的手工用途"),
                None,
                ResourcePrivacy::Public,
                None,
                2,
            )
            .unwrap();
        service
            .upsert_tag(
                resource.id,
                &ResourceTag {
                    name: "手工标签".into(),
                    language: TagLanguage::Zh,
                    source: TagSource::Manual,
                },
                2,
            )
            .unwrap();
        let output = EnrichmentOutput {
            purpose_zh: "AI 用途".into(),
            use_when_zh: "需要图标时".into(),
            capabilities: vec!["搜索图标".into()],
            limitations: vec![],
            categories: vec!["asset-library".into()],
            tags_zh: vec!["图标".into()],
            tags_en: vec!["icon".into()],
            pricing: "unknown".into(),
            requires_login: None,
            languages: vec!["en".into()],
            evidence: vec![Evidence {
                field: "purpose_zh".into(),
                quote: None,
                inferred: true,
            }],
        };
        service.apply_enrichment(resource.id, &output, 3).unwrap();
        let updated = service.get(resource.id).unwrap();
        assert_eq!(updated.purpose_zh.as_deref(), Some("我的手工用途"));
        let tags = service.tags(resource.id).unwrap();
        assert!(
            tags.iter()
                .any(|tag| tag.name == "手工标签" && tag.source == TagSource::Manual)
        );
        assert!(
            tags.iter()
                .any(|tag| tag.name == "icon" && tag.source == TagSource::Ai)
        );
    }

    #[test]
    fn web_clipping_import_is_previewable_idempotent_and_keeps_article() {
        let db = service_db();
        let article_id = db
            .save_web_clipping(
                Some("https://icons.example/tool"),
                Some("Icon tool"),
                "<p>icons</p>",
                1,
            )
            .unwrap();
        let service = ResourceService::new(&db);
        let preview = service.preview_web_clipping_import().unwrap();
        assert_eq!(preview.len(), 1);
        assert!(!preview[0].already_imported);
        assert_eq!(
            service
                .import_web_clippings(&[article_id], 2)
                .unwrap()
                .len(),
            1
        );
        assert!(
            service
                .import_web_clippings(&[article_id], 3)
                .unwrap()
                .is_empty()
        );
        assert!(db.get_article(article_id).is_ok());
        assert!(service.preview_web_clipping_import().unwrap()[0].already_imported);
    }

    #[test]
    fn real_resource_regression_queries_have_an_accepted_top_five_result() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/resource-regression.json"))
                .unwrap();
        let db = service_db();
        let service = ResourceService::new(&db);
        for (index, row) in fixture["resources"].as_array().unwrap().iter().enumerate() {
            let url = row[0].as_str().unwrap();
            let title = row[1].as_str().unwrap();
            let purpose = row[2].as_str().unwrap();
            let resource = service
                .create(&input(url, ResourceSource::Gui), index as i64 + 1)
                .unwrap();
            service
                .update_content(
                    resource.id,
                    Some(title),
                    Some(purpose),
                    None,
                    &[],
                    &[],
                    Some(Pricing::Unknown),
                    None,
                    &[],
                    None,
                    index as i64 + 1,
                )
                .unwrap();
            service
                .transition(resource.id, ResourceStatus::Active, index as i64 + 1)
                .unwrap();
        }
        for row in fixture["queries"].as_array().unwrap() {
            let query = row[0].as_str().unwrap();
            let expected = row[1].as_str().unwrap();
            let results = service.search_json(query, true, false, false, 5).unwrap();
            assert!(
                results.iter().any(|item| item["url"] == expected),
                "query {query:?} missed {expected:?}: {results:?}"
            );
        }
    }
}
