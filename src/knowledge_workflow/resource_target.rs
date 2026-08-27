//! Internal transaction seam between Knowledge Processing and Resource Library.

use anyhow::{Result, bail};
use rusqlite::Connection;

use crate::db::Db;
use crate::library_projection_revision::{self, ProjectionImpact};
use crate::resource_enrichment::{EnrichmentInput, EnrichmentOutput};
use crate::resource_library_lifecycle::{Resource, SnapshotInput};

pub(crate) fn load(db: &Db, resource_id: i64) -> Result<Resource> {
    crate::resource_library_lifecycle::load_processing_resource(db, resource_id)
}

pub(crate) fn enrichment_input(db: &Db, resource_id: i64) -> Result<Option<EnrichmentInput>> {
    crate::resource_library_lifecycle::build_processing_input(db, resource_id)
}

pub(crate) fn record_snapshot_success(
    conn: &Connection,
    resource_id: i64,
    snapshot_id: i64,
    input: &SnapshotInput,
    now: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE resources SET latest_snapshot_id=?2,title=COALESCE(title,?3),
         last_checked_at=?4,updated_at=?4,health='healthy',source_failure_count=0
         WHERE id=?1",
        rusqlite::params![resource_id, snapshot_id, input.title, now],
    )?;
    library_projection_revision::record(conn, ProjectionImpact::resource())?;
    // Library Search owns its derived index and keeps it current through the
    // same transaction via SQLite triggers.
    Ok(())
}

pub(crate) fn record_fetch_failure(
    conn: &Connection,
    resource_id: i64,
    error_kind: &str,
    technical_detail: &str,
    now: i64,
) -> Result<()> {
    if error_kind == "transient" {
        conn.execute(
            "UPDATE resources SET last_checked_at=?2 WHERE id=?1",
            rusqlite::params![resource_id, now],
        )?;
        library_projection_revision::record(conn, ProjectionImpact::resource())?;
        return Ok(());
    }
    let lower = technical_detail.to_ascii_lowercase();
    let permanent = lower.contains("404")
        || lower.contains("410")
        || lower.contains("invalid url")
        || lower.contains("unsupported url");
    conn.execute(
        "UPDATE resources
         SET source_failure_count=source_failure_count+1,
             health=CASE WHEN ?2 OR source_failure_count+1>=3 THEN 'broken' ELSE health END,
             last_checked_at=?3,updated_at=?3
         WHERE id=?1",
        rusqlite::params![resource_id, permanent, now],
    )?;
    library_projection_revision::record(conn, ProjectionImpact::resource())?;
    Ok(())
}

pub(crate) fn apply_enrichment(
    conn: &Connection,
    resource_id: i64,
    output: &EnrichmentOutput,
    now: i64,
) -> Result<()> {
    crate::resource_library_lifecycle::apply_processing_enrichment(conn, resource_id, output, now)?;
    library_projection_revision::record(conn, ProjectionImpact::resource())?;
    Ok(())
}

pub(crate) fn has_active_processing(conn: &Connection, resource_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM knowledge_tasks
           WHERE kind='resource_completion' AND target_id=?1
             AND status IN ('queued','running')
         )",
        [resource_id],
        |row| row.get(0),
    )?)
}

pub(crate) fn prepare_delete(conn: &Connection, resource_id: i64) -> Result<()> {
    if has_active_processing(conn, resource_id)? {
        bail!("RESOURCE_PROCESSING_ACTIVE: {resource_id}");
    }
    conn.execute(
        "DELETE FROM knowledge_tasks
         WHERE kind='resource_completion' AND target_id=?1",
        [resource_id],
    )?;
    Ok(())
}
