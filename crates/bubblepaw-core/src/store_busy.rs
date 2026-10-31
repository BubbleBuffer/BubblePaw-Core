//! Serving-side catalog.db writes that cannot ride in the buffered transaction.
use super::*;
use std::collections::HashMap;

pub(super) fn is_busy(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<rusqlite::Error>()
        .is_some_and(|error| error.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy))
}

impl Store {
    /// The buffer's read snapshot of catalog.db is stale and cannot upgrade
    /// (SQLITE_BUSY_SNAPSHOT). Commit the buffered serving writes without the
    /// catalog rows (the caller's savepoint for them is rolled back here), then
    /// write the rows in their own short transaction, which takes the write lock
    /// at BEGIN.
    pub(super) fn flush_catalog_after_busy(
        &self,
        conn: &Connection,
        pending: &HashMap<String, (String, Session, String)>,
        layout: &layout::Layout,
    ) -> Result<()> {
        conn.execute_batch("ROLLBACK TO catalog_flush; RELEASE catalog_flush; COMMIT")?;
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let rows = pending.values().try_for_each(|(root, session, activity)| {
            Self::catalog_session_to(conn, "catalog_projection.sessions", root, session, activity)
        });
        if let Err(error) = rows {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(error);
        }
        conn.execute_batch("COMMIT")?;
        for _ in 0..2 {
            self.0.transactions.fetch_add(1, Ordering::Relaxed);
            layout.serving_transactions.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "store_busy_tests.rs"]
mod tests;
