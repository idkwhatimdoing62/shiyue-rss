//! Private SQLite persistence helpers for Resource Library Lifecycle.
//!
//! Lifecycle commands and projections live in the parent module. This file
//! only implements the three database-shaped operations needed across sibling
//! module boundaries: search-result serialization, processing input loading,
//! and caller-owned enrichment writes.

use anyhow::{Context, Result, bail};
use reqwest::Url;
use rusqlite::{Row, params};
use serde_json::{Value, json};

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
    Active,
    Broken,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceCurationState {
    PendingReview,
    Active,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceHealth {
    Unknown,
    Healthy,
    Broken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassificationSource {
    Manual,
    Ai,
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

macro_rules! string_enum {
    ($ty:ty, {$($variant:path => $value:literal),+ $(,)?}) => {
        impl $ty {
            pub(super) fn as_str(self) -> &'static str {
                match self { $($variant => $value),+ }
            }

            pub(super) fn parse(value: &str) -> Result<Self> {
                match value {
                    $($value => Ok($variant)),+,
                    _ => bail!("invalid {}: {value}", stringify!($ty)),
                }
            }
        }
    };
}

string_enum!(ResourceKind, {
    ResourceKind::Site => "site",
    ResourceKind::Page => "page",
    ResourceKind::Article => "article"
});
string_enum!(ResourcePrivacy, {
    ResourcePrivacy::Public => "public",
    ResourcePrivacy::Private => "private"
});
string_enum!(ResourceCurationState, {
    ResourceCurationState::PendingReview => "pending_review",
    ResourceCurationState::Active => "active",
    ResourceCurationState::Archived => "archived"
});
string_enum!(ResourceHealth, {
    ResourceHealth::Unknown => "unknown",
    ResourceHealth::Healthy => "healthy",
    ResourceHealth::Broken => "broken"
});
impl ClassificationSource {
    pub(super) fn parse(value: &str) -> Result<Self> {
        match value {
            "manual" => Ok(Self::Manual),
            "ai" => Ok(Self::Ai),
            _ => bail!("invalid ClassificationSource: {value}"),
        }
    }
}
string_enum!(ResourceSource, {
    ResourceSource::Gui => "gui",
    ResourceSource::CliAgent => "cli_agent",
    ResourceSource::Import => "import"
});
string_enum!(Pricing, {
    Pricing::Free => "free",
    Pricing::Freemium => "freemium",
    Pricing::Paid => "paid",
    Pricing::Unknown => "unknown"
});
string_enum!(Category, {
    Category::Tool => "tool",
    Category::AssetLibrary => "asset-library",
    Category::Docs => "docs",
    Category::Blog => "blog",
    Category::Inspiration => "inspiration",
    Category::Service => "service",
    Category::Repository => "repository",
    Category::Other => "other"
});
string_enum!(TagSource, {
    TagSource::Manual => "manual",
    TagSource::Ai => "ai"
});
string_enum!(TagLanguage, {
    TagLanguage::Zh => "zh",
    TagLanguage::En => "en",
    TagLanguage::Other => "other"
});

impl ResourceStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::PendingReview => "pending_review",
            Self::Active => "active",
            Self::Broken => "broken",
            Self::Archived => "archived",
        }
    }
}

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
    pub curation_state: ResourceCurationState,
    pub health: ResourceHealth,
    pub categories_source: ClassificationSource,
    pub tags_source: ClassificationSource,
    pub source_failure_count: i64,
    pub source: ResourceSource,
    pub manual_rating: Option<i64>,
    pub latest_snapshot_id: Option<i64>,
    pub last_checked_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct SnapshotInput {
    pub fetched_url: Option<String>,
    pub http_status: Option<i64>,
    pub title: Option<String>,
    pub cleaned_content: Option<String>,
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

pub(super) fn enrichment_input(
    db: &Db,
    id: i64,
) -> Result<Option<crate::resource_enrichment::EnrichmentInput>> {
    let resource = db
        .conn
        .query_row(
            &format!("SELECT {RESOURCE_COLS} FROM resources WHERE id=?1"),
            [id],
            map_resource,
        )
        .context("resource not found")?;
    if resource.privacy == ResourcePrivacy::Private {
        return Ok(None);
    }
    let content = if let Some(snapshot_id) = resource.latest_snapshot_id {
        db.conn.query_row(
            "SELECT COALESCE(cleaned_content,'') FROM resource_snapshots WHERE id=?1",
            [snapshot_id],
            |row| row.get::<_, String>(0),
        )?
    } else if let Some(article_id) = resource.linked_article_id {
        db.conn.query_row(
            "SELECT COALESCE(content,'') FROM articles WHERE id=?1",
            [article_id],
            |row| row.get::<_, String>(0),
        )?
    } else {
        String::new()
    };
    Ok(Some(crate::resource_enrichment::EnrichmentInput {
        resource_id: id,
        // Re-validate persisted data before crossing the provider boundary.
        // Older databases may contain URLs created before userinfo was rejected.
        url: canonicalize_url(&resource.url)?,
        title: resource.title,
        // Private notes are local-only metadata and are never sent to providers.
        private_note: None,
        cleaned_content: content,
    }))
}

/// Apply an AI result through a caller-owned transaction. Knowledge Processing
/// uses this to commit the business result and fenced task state atomically.
pub(super) fn apply_enrichment_on(
    conn: &rusqlite::Connection,
    id: i64,
    output: &crate::resource_enrichment::EnrichmentOutput,
    now: i64,
) -> Result<()> {
    let capabilities = serde_json::to_string(&output.capabilities)?;
    let limitations = serde_json::to_string(&output.limitations)?;
    let languages = serde_json::to_string(&output.languages)?;
    let (categories_manual, tags_manual) = conn.query_row(
        "SELECT categories_source='manual',tags_source='manual' FROM resources WHERE id=?1",
        [id],
        |row| Ok((row.get::<_, bool>(0)?, row.get::<_, bool>(1)?)),
    )?;
    conn.execute(
        "UPDATE resources SET
           purpose_zh=CASE WHEN purpose_source='manual' THEN purpose_zh ELSE ?2 END,
           purpose_source=CASE WHEN purpose_source='manual' THEN 'manual' ELSE 'ai' END,
           use_when_zh=CASE WHEN use_when_source='manual' THEN use_when_zh ELSE ?3 END,
           use_when_source=CASE WHEN use_when_source='manual' THEN 'manual' ELSE 'ai' END,
           capabilities=?4,limitations=?5,pricing=?6,requires_login=?7,languages=?8,updated_at=?9
         WHERE id=?1",
        params![
            id,
            output.purpose_zh,
            output.use_when_zh,
            capabilities,
            limitations,
            output.pricing,
            output.requires_login,
            languages,
            now
        ],
    )?;
    if !categories_manual {
        conn.execute("DELETE FROM resource_categories WHERE resource_id=?1", [id])?;
        for category in &output.categories {
            Category::parse(category)?;
            conn.execute(
                "INSERT INTO resource_categories(resource_id,category) VALUES(?1,?2)",
                params![id, category],
            )?;
        }
    }
    if !tags_manual {
        conn.execute(
            "DELETE FROM resource_tags WHERE resource_id=?1 AND source='ai'",
            [id],
        )?;
        for (name, language) in output
            .tags_zh
            .iter()
            .map(|value| (value, "zh"))
            .chain(output.tags_en.iter().map(|value| (value, "en")))
        {
            if name.trim().is_empty() || name.chars().count() > 100 {
                bail!("invalid AI tag")
            }
            conn.execute(
                "INSERT INTO resource_tags(resource_id,name,language,source,created_at)
                 VALUES(?1,?2,?3,'ai',?4)
                 ON CONFLICT(resource_id,name) DO NOTHING",
                params![id, name, language, now],
            )?;
        }
    }
    Ok(())
}

pub(super) fn resource_json(
    db: &Db,
    resource: &Resource,
    matched_field: &str,
    snippet: String,
    score: f64,
) -> Result<Value> {
    let mut value = base_resource_json(resource, matched_field, snippet, score);
    let categories = {
        let mut statement = db.conn.prepare(
            "SELECT category FROM resource_categories WHERE resource_id=?1 ORDER BY category",
        )?;
        statement
            .query_map([resource.id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let tags = {
        let mut statement = db.conn.prepare(
            "SELECT name,language,source FROM resource_tags
             WHERE resource_id=?1 ORDER BY name COLLATE NOCASE",
        )?;
        let rows = statement.query_map([resource.id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (name, language, source) = row?;
            Ok(json!({
                "name": name,
                "language": TagLanguage::parse(&language)?.as_str(),
                "source": TagSource::parse(&source)?.as_str(),
            }))
        })
        .collect::<Result<Vec<_>>>()?
    };
    value["categories"] = json!(categories);
    value["tags"] = json!(tags);
    Ok(value)
}

fn base_resource_json(resource: &Resource, field: &str, snippet: String, score: f64) -> Value {
    json!({
        "id": resource.id.to_string(),
        "result_type": "resource",
        "url": resource.url,
        "title": resource.title,
        "kind": resource.kind.as_str(),
        "categories": [],
        "tags": [],
        "purpose_zh": resource.purpose_zh,
        "use_when_zh": resource.use_when_zh,
        "capabilities": resource.capabilities,
        "limitations": resource.limitations,
        "pricing": resource.pricing.map(Pricing::as_str),
        "requires_login": resource.requires_login,
        "private_note": resource.private_note.as_ref().map(|value| json!({
            "value": value,
            "source": "local_private_note"
        })),
        "matched_fields": [field],
        "evidence_snippets": [{
            "source_type": field,
            "snapshot_id": resource.latest_snapshot_id.map(|value| value.to_string()),
            "article_id": resource.linked_article_id.map(|value| value.to_string()),
            "text": snippet
        }],
        "updated_at": rfc3339(resource.updated_at),
        "last_checked_at": resource.last_checked_at.map(rfc3339),
        "status": resource.status.as_str(),
        "curation_state": resource.curation_state.as_str(),
        "health": resource.health.as_str(),
        "score": score,
        "score_factors": [
            "text_match",
            if resource.manual_rating.is_some() {
                "manual_rating_boost"
            } else {
                "no_rating_boost"
            }
        ]
    })
}

fn rfc3339(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub(super) const RESOURCE_COLS: &str = "id,url,canonical_url,parent_resource_id,linked_article_id,kind,title,purpose_zh,use_when_zh,capabilities,limitations,pricing,requires_login,languages,private_note,privacy,source,manual_rating,latest_snapshot_id,last_checked_at,created_at,updated_at,curation_state,health,categories_source,tags_source,source_failure_count";

pub(super) fn map_resource(row: &Row) -> rusqlite::Result<Resource> {
    fn conversion(error: anyhow::Error) -> rusqlite::Error {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, error.into())
    }

    let kind = ResourceKind::parse(&row.get::<_, String>(5)?).map_err(conversion)?;
    let pricing = row
        .get::<_, Option<String>>(11)?
        .map(|value| Pricing::parse(&value))
        .transpose()
        .map_err(conversion)?;
    let curation_state =
        ResourceCurationState::parse(&row.get::<_, String>(22)?).map_err(conversion)?;
    let health = ResourceHealth::parse(&row.get::<_, String>(23)?).map_err(conversion)?;
    let status = match (curation_state, health) {
        (ResourceCurationState::PendingReview, _) => ResourceStatus::PendingReview,
        (ResourceCurationState::Archived, _) => ResourceStatus::Archived,
        (_, ResourceHealth::Broken) => ResourceStatus::Broken,
        _ => ResourceStatus::Active,
    };
    Ok(Resource {
        id: row.get(0)?,
        url: row.get(1)?,
        canonical_url: row.get(2)?,
        parent_resource_id: row.get(3)?,
        linked_article_id: row.get(4)?,
        kind,
        title: row.get(6)?,
        purpose_zh: row.get(7)?,
        use_when_zh: row.get(8)?,
        capabilities: serde_json::from_str(&row.get::<_, String>(9)?)
            .map_err(|error| conversion(error.into()))?,
        limitations: serde_json::from_str(&row.get::<_, String>(10)?)
            .map_err(|error| conversion(error.into()))?,
        pricing,
        requires_login: row.get(12)?,
        languages: serde_json::from_str(&row.get::<_, String>(13)?)
            .map_err(|error| conversion(error.into()))?,
        private_note: row.get(14)?,
        privacy: ResourcePrivacy::parse(&row.get::<_, String>(15)?).map_err(conversion)?,
        status,
        source: ResourceSource::parse(&row.get::<_, String>(16)?).map_err(conversion)?,
        manual_rating: row.get(17)?,
        latest_snapshot_id: row.get(18)?,
        last_checked_at: row.get(19)?,
        created_at: row.get(20)?,
        updated_at: row.get(21)?,
        curation_state,
        health,
        categories_source: ClassificationSource::parse(&row.get::<_, String>(24)?)
            .map_err(conversion)?,
        tags_source: ClassificationSource::parse(&row.get::<_, String>(25)?).map_err(conversion)?,
        source_failure_count: row.get(26)?,
    })
}

pub(crate) fn canonicalize_url(raw: &str) -> Result<String> {
    let mut url = Url::parse(raw.trim()).context("invalid resource URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("resource URL must use HTTP(S)")
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("resource URL must not contain credentials")
    }
    url.set_fragment(None);
    let remove_port = (url.scheme() == "http" && url.port() == Some(80))
        || (url.scheme() == "https" && url.port() == Some(443));
    if remove_port {
        url.set_port(None)
            .map_err(|_| anyhow::anyhow!("invalid port"))?;
    }
    if url.path().is_empty() {
        url.set_path("/");
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::canonicalize_url;

    #[test]
    fn canonicalize_url_rejects_embedded_credentials() {
        assert!(canonicalize_url("https://alice:secret@example.com/path").is_err());
        assert!(canonicalize_url("https://alice@example.com/path").is_err());
    }
}
