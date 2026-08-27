//! Durable revision vector for desktop library projections (ADR-0014).
//!
//! SQLite remains the source of truth. Writers bump the affected family in
//! the same transaction as the durable change; projection readers use that
//! monotonic position to reject stale or regressing results.

use anyhow::Context as _;
use rusqlite::Connection;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ProjectionFamily {
    Article,
    Resource,
    Excerpt,
}

/// Closed, transaction-local description of the projection families whose
/// observable durable material changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ProjectionImpact {
    article: bool,
    resource: bool,
    excerpt: bool,
}

impl ProjectionImpact {
    pub(crate) const fn none() -> Self {
        Self {
            article: false,
            resource: false,
            excerpt: false,
        }
    }

    pub(crate) const fn article() -> Self {
        Self {
            article: true,
            ..Self::none()
        }
    }

    pub(crate) const fn resource() -> Self {
        Self {
            resource: true,
            ..Self::none()
        }
    }

    pub(crate) const fn excerpt() -> Self {
        Self {
            excerpt: true,
            ..Self::none()
        }
    }

    pub(crate) const fn with(mut self, family: ProjectionFamily) -> Self {
        match family {
            ProjectionFamily::Article => self.article = true,
            ProjectionFamily::Resource => self.resource = true,
            ProjectionFamily::Excerpt => self.excerpt = true,
        }
        self
    }

    pub(crate) const fn is_empty(self) -> bool {
        !self.article && !self.resource && !self.excerpt
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LibraryProjectionRevision {
    pub(crate) article: i64,
    pub(crate) resource: i64,
    pub(crate) excerpt: i64,
}

impl LibraryProjectionRevision {
    pub(crate) fn family(self, family: ProjectionFamily) -> i64 {
        match family {
            ProjectionFamily::Article => self.article,
            ProjectionFamily::Resource => self.resource,
            ProjectionFamily::Excerpt => self.excerpt,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct LibraryGeneration(String);

impl LibraryGeneration {
    pub(crate) fn from_epoch(epoch: &str) -> Self {
        Self(epoch.to_owned())
    }

    pub(crate) fn uncoordinated() -> Self {
        Self("uncoordinated".into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectionStamp {
    pub(crate) generation: LibraryGeneration,
    pub(crate) revision: i64,
}

pub(crate) fn migrate_to_v8(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE library_projection_revisions (
           singleton_id     INTEGER PRIMARY KEY CHECK(singleton_id = 1),
           article_revision INTEGER NOT NULL DEFAULT 0 CHECK(article_revision >= 0),
           resource_revision INTEGER NOT NULL DEFAULT 0 CHECK(resource_revision >= 0),
           excerpt_revision INTEGER NOT NULL DEFAULT 0 CHECK(excerpt_revision >= 0)
         );
         INSERT INTO library_projection_revisions(
           singleton_id, article_revision, resource_revision, excerpt_revision
         ) VALUES(1, 0, 0, 0);",
    )?;
    Ok(())
}

pub(crate) fn verify_schema_v8(conn: &Connection) -> anyhow::Result<()> {
    let columns = conn
        .prepare("PRAGMA table_info(library_projection_revisions)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<std::collections::BTreeSet<_>>>()?;
    for required in [
        "singleton_id",
        "article_revision",
        "resource_revision",
        "excerpt_revision",
    ] {
        anyhow::ensure!(
            columns.contains(required),
            "SCHEMA_MISSING_COLUMN: library_projection_revisions.{required}"
        );
    }
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM library_projection_revisions WHERE singleton_id=1",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(count == 1, "PROJECTION_REVISION_SINGLETON_MISSING");
    Ok(())
}

pub(crate) fn read(conn: &Connection) -> anyhow::Result<LibraryProjectionRevision> {
    conn.query_row(
        "SELECT article_revision, resource_revision, excerpt_revision
         FROM library_projection_revisions WHERE singleton_id=1",
        [],
        |row| {
            Ok(LibraryProjectionRevision {
                article: row.get(0)?,
                resource: row.get(1)?,
                excerpt: row.get(2)?,
            })
        },
    )
    .context("read library projection revision")
}

pub(crate) fn read_family(conn: &Connection, family: ProjectionFamily) -> anyhow::Result<i64> {
    Ok(read(conn)?.family(family))
}

#[cfg(test)]
pub(crate) fn bump(conn: &Connection, family: ProjectionFamily) -> anyhow::Result<i64> {
    Ok(record(conn, ProjectionImpact::none().with(family))?.family(family))
}

/// Records the complete impact of one durable transaction in a single
/// revision-vector update. An empty impact observes the current vector.
pub(crate) fn record(
    conn: &Connection,
    impact: ProjectionImpact,
) -> anyhow::Result<LibraryProjectionRevision> {
    if impact.is_empty() {
        return read(conn);
    }
    let changed = conn.execute(
        "UPDATE library_projection_revisions
         SET article_revision=article_revision+?1,
             resource_revision=resource_revision+?2,
             excerpt_revision=excerpt_revision+?3
         WHERE singleton_id=1",
        [
            i64::from(impact.article),
            i64::from(impact.resource),
            i64::from(impact.excerpt),
        ],
    )?;
    anyhow::ensure!(changed == 1, "PROJECTION_REVISION_SINGLETON_MISSING");
    read(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_revisions_advance_independently_and_monotonically() {
        let conn = Connection::open_in_memory().unwrap();
        migrate_to_v8(&conn).unwrap();
        assert_eq!(read(&conn).unwrap(), LibraryProjectionRevision::default());
        assert_eq!(bump(&conn, ProjectionFamily::Resource).unwrap(), 1);
        assert_eq!(bump(&conn, ProjectionFamily::Resource).unwrap(), 2);
        assert_eq!(bump(&conn, ProjectionFamily::Article).unwrap(), 1);
        assert_eq!(
            read(&conn).unwrap(),
            LibraryProjectionRevision {
                article: 1,
                resource: 2,
                excerpt: 0,
            }
        );
    }

    #[test]
    fn impact_is_closed_deduplicated_and_recorded_atomically() {
        let conn = Connection::open_in_memory().unwrap();
        migrate_to_v8(&conn).unwrap();
        let impact = ProjectionImpact::article()
            .with(ProjectionFamily::Excerpt)
            .with(ProjectionFamily::Article);
        assert_eq!(
            record(&conn, impact).unwrap(),
            LibraryProjectionRevision {
                article: 1,
                resource: 0,
                excerpt: 1,
            }
        );
        assert_eq!(record(&conn, ProjectionImpact::none()).unwrap().article, 1);
    }
}
