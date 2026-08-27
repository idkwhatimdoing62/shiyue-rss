//! Internal transaction seam between Knowledge Processing and Article owners.

use anyhow::{Result, bail};
use rusqlite::Connection;

pub(crate) fn has_active_summary(conn: &Connection, article_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM knowledge_tasks
           WHERE kind='article_summary' AND target_id=?1
             AND status IN ('queued','running')
         )",
        [article_id],
        |row| row.get(0),
    )?)
}

/// Reject deletion while an Article Summary can still write, then remove only
/// terminal workflow history. The caller owns deletion of the Article itself.
pub(crate) fn prepare_delete(conn: &Connection, article_id: i64) -> Result<()> {
    if has_active_summary(conn, article_id)? {
        bail!("ARTICLE_SUMMARY_ACTIVE: {article_id}");
    }
    conn.execute(
        "DELETE FROM knowledge_tasks
         WHERE kind='article_summary' AND target_id=?1",
        [article_id],
    )?;
    Ok(())
}
