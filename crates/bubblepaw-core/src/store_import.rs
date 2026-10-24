//! Imported provenance and derived source records stay with the canonical chat.
use super::*;
use crate::transcript::ImportDerivedRecord;

const IMPORT_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS imported_derived(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,source_line INTEGER NOT NULL,kind TEXT NOT NULL,boundary_seq INTEGER,data TEXT NOT NULL); CREATE INDEX IF NOT EXISTS imported_derived_boundary ON imported_derived(session_id,boundary_seq); CREATE INDEX IF NOT EXISTS imported_summary_latest ON imported_derived(session_id,source_line DESC,id DESC) WHERE kind IN ('codex_compaction','claude_compaction_summary') AND boundary_seq IS NOT NULL;";

impl Store {
    /// Commit a bounded batch of imported messages and source-derived records.
    pub fn append_import_batch(
        &self,
        messages: &[Message],
        derived: &[ImportDerivedRecord],
    ) -> Result<()> {
        let session = messages
            .first()
            .map(|m| m.session_id.as_str())
            .or_else(|| derived.first().map(|d| d.session_id.as_str()));
        let Some(session) = session else {
            return Ok(());
        };
        anyhow::ensure!(
            messages.iter().all(|m| m.session_id == session)
                && derived.iter().all(|d| d.session_id == session),
            "An import batch must belong to one session"
        );
        if let Some(chat) = self.scoped(session)? {
            chat.append_import_batch(messages, derived)?;
            // Imports can run for gigabytes without an idle runtime boundary.
            // Bound the disposable FTS page cache to one parsed batch too.
            return self.flush_serving();
        }
        self.write(|tx| {
            tx.execute_batch(IMPORT_SCHEMA)?;
            for message in messages { Self::insert_message_with_blobs(tx, message, true)?; }
            for record in derived {
                anyhow::ensure!(record.origin == "import", "Imported derived records require origin import");
                let boundary: Option<i64> = record.through_message_id.as_deref().map(|id| Self::session_message_seq(tx, session, id)).transpose()?;
                tx.execute("INSERT INTO imported_derived(id,session_id,source_line,kind,boundary_seq,data) VALUES(?1,?2,?3,?4,?5,?6)", params![record.id,session,i64::try_from(record.source_line)?,record.source_kind,boundary,serde_json::to_string(record)?])?;
            }
            Ok(())
        })
    }

    /// Final import receipt; stored inside the chat, so catalog rebuilding retains it.
    pub fn set_import_metadata(&self, session: &str, metadata: &serde_json::Value) -> Result<()> {
        if let Some(chat) = self.scoped(session)? {
            return chat.set_import_metadata(session, metadata);
        }
        self.session(session)?;
        self.write(|tx| {
            tx.execute_batch(IMPORT_SCHEMA)?;
            tx.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![format!("import:{session}"),serde_json::to_string(metadata)?])?;
            Ok(())
        })
    }

    pub fn import_metadata(&self, session: &str) -> Result<Option<serde_json::Value>> {
        if let Some(chat) = self.scoped(session)? {
            return chat.import_metadata(session);
        }
        self.session(session)?;
        self.read(|conn| {
            let data: Option<String> = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [format!("import:{session}")],
                    |row| row.get(0),
                )
                .optional()?;
            data.map(|data| serde_json::from_str(&data).map_err(Into::into))
                .transpose()
        })
    }

    pub fn import_derived(&self, session: &str) -> Result<Vec<ImportDerivedRecord>> {
        if let Some(chat) = self.scoped(session)? {
            return chat.import_derived(session);
        }
        self.session(session)?;
        self.read(|conn| {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='imported_derived')",
                [],
                |row| row.get(0),
            )?;
            if !exists {
                return Ok(Vec::new());
            }
            let mut statement = conn.prepare(
                "SELECT data FROM imported_derived WHERE session_id=?1 ORDER BY source_line,id",
            )?;
            statement
                .query_map([session], |row| row.get::<_, String>(0))?
                .map(|row| serde_json::from_str(&row?).map_err(Into::into))
                .collect()
        })
    }

    /// Decode one imported derived record at a time without retaining its neighbours.
    /// The callback must not reenter this Store while its read lock is held.
    pub fn for_each_import_derived(
        &self,
        session: &str,
        mut visit: impl FnMut(ImportDerivedRecord) -> Result<()>,
    ) -> Result<usize> {
        if let Some(chat) = self.scoped(session)? {
            return chat.for_each_import_derived(session, visit);
        }
        self.read(|conn| {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='imported_derived')",
                [],
                |row| row.get(0),
            )?;
            if !exists {
                return Ok(0);
            }
            let mut statement = conn.prepare(
                "SELECT data FROM imported_derived WHERE session_id=?1 ORDER BY source_line,id",
            )?;
            let mut rows = statement.query([session])?;
            let mut count = 0;
            while let Some(row) = rows.next()? {
                visit(serde_json::from_str(&row.get::<_, String>(0)?)?)?;
                count += 1;
            }
            Ok(count)
        })
    }
}
