//! The context feature's tables. The rest of the store asks this module which
//! of them to delete, scan for blob references, read for cold storage and split
//! telemetry from; it does not name them itself.
use super::*;

/// The context feature's tables, in one place.
pub(in crate::store) struct ContextTables;

impl ContextTables {
    /// The per-session view table.
    pub(in crate::store) const VIEWS: &'static str = "context_views";

    /// Tables to delete for a session, in deletion order. The view table comes
    /// first when `with_views` is set.
    pub(in crate::store) fn session_delete(with_views: bool) -> Vec<&'static str> {
        let mut tables = Vec::new();
        if with_views {
            tables.push(Self::VIEWS);
        }
        tables.extend([
            "context_checkpoint_versions",
            "context_decisions",
            "context_events",
            "context_episodes",
            "context_subject_chapters",
            "context_chapters",
            "context_checkpoints",
        ]);
        tables
    }

    /// `(table, column)` pairs whose text can name a blob hash.
    pub(in crate::store) fn blob_references() -> [(&'static str, &'static str); 2] {
        [
            ("context_checkpoints", "data"),
            ("context_checkpoint_versions", "summary_hash"),
        ]
    }

    /// `(table, boundary column)` pairs that mark how far a session's history is
    /// covered by derived records.
    pub(in crate::store) fn frontier_boundaries() -> [(&'static str, &'static str); 3] {
        [
            ("context_checkpoints", "through_message_seq"),
            ("context_chapters", "last_message_seq"),
            ("context_episodes", "last_message_seq"),
        ]
    }

    /// The newest message sequence covered by any derived record of the session.
    pub(in crate::store) fn latest_boundary(
        conn: &Connection,
        session: &str,
    ) -> Result<Option<i64>> {
        Ok(conn.query_row("SELECT MAX(boundary) FROM (SELECT through_message_seq AS boundary FROM context_checkpoints WHERE session_id=?1 UNION ALL SELECT last_message_seq FROM context_chapters WHERE session_id=?1 UNION ALL SELECT last_message_seq FROM context_episodes WHERE session_id=?1)", [session], |row| row.get(0))?)
    }

    /// The stored view data of the session.
    pub(in crate::store) fn view_data(conn: &Connection, session: &str) -> Result<Option<String>> {
        Ok(conn
            .query_row(
                "SELECT data FROM context_views WHERE session_id=?1",
                [session],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Tables whose rows carry usage and reasoning that move to telemetry.
    pub(in crate::store) fn telemetry_split() -> [&'static str; 5] {
        [
            "context_checkpoints",
            "context_chapters",
            "context_episodes",
            "context_subject_chapters",
            "context_decisions",
        ]
    }
}

#[cfg(test)]
#[path = "store_context_tables_tests.rs"]
mod tests;
