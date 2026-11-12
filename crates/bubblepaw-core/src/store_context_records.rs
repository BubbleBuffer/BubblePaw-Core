//! Context checkpoints, chapters, episodes and their coverage records.
use super::*;

impl Store {
    /// Read the latest serving checkpoint without opening a write transaction.
    pub fn context_checkpoint(&self, session_id: &str) -> Result<Option<ContextCheckpoint>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.context_checkpoint(session_id);
        }
        let conn = self.0.connection.lock().unwrap();
        let value: Option<String> = conn
            .query_row(
                "SELECT data FROM context_checkpoints WHERE session_id=?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        value
            .map(|value| Self::decode_checkpoint(&conn, &self.hydrate_context_telemetry(&value)?))
            .transpose()
    }

    /// Read immutable chapters in session order without a write transaction.
    pub fn context_chapters(&self, session_id: &str) -> Result<Vec<ContextChapter>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.context_chapters(session_id);
        }
        let conn = self.0.connection.lock().unwrap();
        let mut statement =
            conn.prepare("SELECT data FROM context_chapters WHERE session_id=?1 ORDER BY ordinal")?;
        statement
            .query_map([session_id], |row| row.get::<_, String>(0))?
            .map(|value| {
                Ok(serde_json::from_str(
                    &self.hydrate_context_telemetry(&value?)?,
                )?)
            })
            .collect()
    }

    pub fn context_episodes(&self, session_id: &str) -> Result<Vec<ContextEpisode>> {
        self.context_records("context_episodes", "ordinal", session_id)
    }

    /// Read the frozen view without starting a write transaction.
    pub fn context_view(&self, session_id: &str) -> Result<Option<ContextView>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.context_view(session_id);
        }
        if let Some(host) = self.host() {
            let data: Option<String> = host.read(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT data FROM context_views WHERE session_id=?1",
                        [session_id],
                        |row| row.get(0),
                    )
                    .optional()?)
            })?;
            return data
                .map(|data| self.read(|conn| Self::decode_context_view(conn, &data)))
                .transpose();
        }
        let conn = self.0.connection.lock().unwrap();
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM context_views WHERE session_id=?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        data.map(|data| Self::decode_context_view(&conn, &data))
            .transpose()
    }

    fn decode_checkpoint(conn: &Connection, data: &str) -> Result<ContextCheckpoint> {
        let mut value: serde_json::Value = serde_json::from_str(data)?;
        if let Some(hash) = value.get("_summary_blob").and_then(|value| value.as_str()) {
            value["summary"] =
                serde_json::Value::String(String::from_utf8(Self::blob_get(conn, hash)?)?);
        }
        Ok(serde_json::from_value(value)?)
    }

    fn archive_checkpoint(conn: &Connection, checkpoint: &ContextCheckpoint) -> Result<String> {
        let hash = Self::blob_put(conn, checkpoint.summary.as_bytes())?;
        conn.execute("INSERT OR IGNORE INTO context_checkpoint_versions(id,session_id,summary_hash) VALUES(?1,?2,?3)", params![checkpoint.id, checkpoint.session_id, hash])?;
        let existing: String = conn.query_row(
            "SELECT summary_hash FROM context_checkpoint_versions WHERE id=?1 AND session_id=?2",
            params![checkpoint.id, checkpoint.session_id],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            existing == hash,
            "Checkpoint IDs identify immutable summaries"
        );
        Ok(hash)
    }

    pub(super) fn archive_current_checkpoint(conn: &Connection, session_id: &str) -> Result<()> {
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM context_checkpoints WHERE session_id=?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(data) = data {
            Self::archive_checkpoint(conn, &Self::decode_checkpoint(conn, &data)?)?;
        }
        Ok(())
    }

    /// Resolve a canonical message ID to its global SQLite sequence.
    pub fn message_seq(&self, session_id: &str, message_id: &str) -> Result<i64> {
        if let Some(store) = self.scoped(session_id)? {
            return store.message_seq(session_id, message_id);
        }
        let conn = self.0.connection.lock().unwrap();
        Self::session_message_seq(&conn, session_id, message_id)
    }

    /// Resolve a view's inclusive boundary back to a canonical message ID.
    pub fn context_view_frontier_message_id(
        &self,
        session_id: &str,
        frontier: Option<i64>,
    ) -> Result<Option<String>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.context_view_frontier_message_id(session_id, frontier);
        }
        let conn = self.0.connection.lock().unwrap();
        frontier
            .map(|seq| {
                conn.query_row(
                    "SELECT message_id FROM messages WHERE session_id=?1 AND seq=?2",
                    params![session_id, seq],
                    |row| row.get(0),
                )
                .optional()?
                .context("View frontier does not belong to the session")
            })
            .transpose()
    }

    pub(super) fn session_from_connection(conn: &Connection, session_id: &str) -> Result<Session> {
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM sessions WHERE id=?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(serde_json::from_str(
            &data.context("Context session does not exist")?,
        )?)
    }

    /// A coverage boundary must not separate an assistant call from its results.
    pub(super) fn validate_complete_frontier(
        conn: &Connection,
        session_id: &str,
        frontier: i64,
    ) -> Result<()> {
        // The source's own compaction closes its prior serving round. Original
        // unmatched source calls remain in history and the no-reuse registry;
        // they are not BubblePaw effects and cannot block later native summaries.
        let source = Self::imported_history_seed_at(conn, session_id)?
            .0
            .filter(|source| *source <= frontier)
            .unwrap_or(0);
        let mut statement = conn.prepare("SELECT call_id FROM message_tool_calls WHERE session_id=?1 AND message_seq>?2 AND message_seq<=?3 GROUP BY call_id HAVING MAX(CASE WHEN kind='call' THEN message_seq END)>COALESCE(MAX(CASE WHEN kind='result' THEN message_seq END),0)")?;
        let pending = statement
            .query_map(params![session_id, source, frontier], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        anyhow::ensure!(
            pending.is_empty(),
            "Context frontier must preserve complete tool rounds"
        );
        Ok(())
    }

    pub fn context_subject_chapters(&self, session_id: &str) -> Result<Vec<ContextSubjectChapter>> {
        self.context_records("context_subject_chapters", "ordinal", session_id)
    }

    pub fn context_decisions(&self, session_id: &str) -> Result<Vec<ContextDecision>> {
        self.context_records("context_decisions", "episode_ordinal,rowid", session_id)
    }

    fn context_records<T: serde::de::DeserializeOwned>(
        &self,
        table: &str,
        order: &str,
        session_id: &str,
    ) -> Result<Vec<T>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.context_records(table, order, session_id);
        }
        let conn = self.0.connection.lock().unwrap();
        let mut statement = conn.prepare(&format!(
            "SELECT data FROM {table} WHERE session_id=?1 ORDER BY {order}"
        ))?;
        statement
            .query_map([session_id], |row| row.get::<_, String>(0))?
            .map(|value| {
                Ok(serde_json::from_str(
                    &self.hydrate_context_telemetry(&value?)?,
                )?)
            })
            .collect()
    }

    pub fn open_subject_chapter(&self, session_id: &str) -> Result<Option<ContextSubjectChapter>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.open_subject_chapter(session_id);
        }
        let conn = self.0.connection.lock().unwrap();
        Self::read_open_subject_chapter(&conn, session_id)?
            .map(|chapter| {
                let data = self.hydrate_context_telemetry(&serde_json::to_string(&chapter)?)?;
                Ok(serde_json::from_str(&data)?)
            })
            .transpose()
    }

    pub(super) fn read_open_subject_chapter(
        conn: &Connection,
        session_id: &str,
    ) -> Result<Option<ContextSubjectChapter>> {
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM context_subject_chapters WHERE session_id=?1 AND status='open'",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        data.map(|data| Ok(serde_json::from_str(&data)?))
            .transpose()
    }

    /// Last covered canonical message, including frozen legacy coverage.
    pub fn episode_coverage_token(&self, session_id: &str) -> Result<Option<String>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.episode_coverage_token(session_id);
        }
        let conn = self.0.connection.lock().unwrap();
        Ok(Self::episode_coverage(&conn, session_id)?.2)
    }

    pub(super) fn episode_coverage(
        conn: &Connection,
        session_id: &str,
    ) -> Result<(i64, i64, Option<String>)> {
        let latest: Option<(i64, i64)> = conn
            .query_row(
                "SELECT ordinal,last_message_seq FROM context_episodes WHERE session_id=?1 ORDER BY ordinal DESC LIMIT 1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (ordinal, last) = match latest {
            Some(latest) => latest,
            None => {
                let cutoff: i64 = conn.query_row(
                    "SELECT COALESCE((SELECT last_message_seq FROM context_chapters WHERE session_id=?1 ORDER BY ordinal DESC LIMIT 1),(SELECT through_message_seq FROM context_checkpoints WHERE session_id=?1),0)",
                    [session_id],
                    |row| row.get(0),
                )?;
                let cutoff = if cutoff == 0 {
                    Self::imported_history_seed_at(conn, session_id)?
                        .0
                        .unwrap_or(0)
                } else {
                    cutoff
                };
                (0, cutoff)
            }
        };
        let token = if last == 0 {
            None
        } else {
            Some(conn.query_row(
                "SELECT message_id FROM messages WHERE session_id=?1 AND seq=?2",
                params![session_id, last],
                |row| row.get(0),
            )?)
        };
        Ok((ordinal, last, token))
    }

    /// Append one chapter atomically. Existing chapters and original messages
    /// are never replaced. The host enforces its model-specific token limit;
    /// this boundary also bounds durable summary bytes.
    pub fn save_context_chapter(&self, chapter: &ContextChapter) -> Result<()> {
        if let Some(store) = self.scoped(&chapter.session_id)? {
            return store.save_context_chapter(chapter);
        }
        anyhow::ensure!(!chapter.id.trim().is_empty(), "Chapter ID is empty");
        anyhow::ensure!(
            !chapter.session_id.trim().is_empty(),
            "Chapter session ID is empty"
        );
        anyhow::ensure!(
            !chapter.first_message_id.trim().is_empty()
                && !chapter.last_message_id.trim().is_empty(),
            "Chapter message range is empty"
        );
        let ordinal = i64::try_from(chapter.ordinal).context("Chapter ordinal is too large")?;
        anyhow::ensure!(ordinal > 0, "Chapter ordinal must be one-based");
        anyhow::ensure!(
            !chapter.title.trim().is_empty()
                && chapter.title.chars().count() <= 80
                && !chapter.title.contains(['\r', '\n']),
            "Chapter title must be nonempty, single-line and at most 80 characters"
        );
        anyhow::ensure!(
            !chapter.summary.trim().is_empty() && chapter.summary.len() <= 64 * 1024,
            "Chapter summary must be nonempty and at most 65536 bytes"
        );
        anyhow::ensure!(!chapter.model.trim().is_empty(), "Chapter model is empty");
        anyhow::ensure!(
            !chapter.created_at.trim().is_empty(),
            "Chapter creation time is empty"
        );
        let data = serde_json::to_string(chapter)?;
        self.write_context(|tx| {
            let message_seq = |message_id: &str| -> Result<i64> {
                tx.query_row(
                    "SELECT seq FROM messages WHERE session_id=?1 AND message_id=?2",
                    params![chapter.session_id, message_id],
                    |row| row.get(0),
                )
                .optional()?
                .context("Chapter range endpoint does not belong to the session")
            };
            let first = message_seq(&chapter.first_message_id)?;
            let last = message_seq(&chapter.last_message_id)?;
            anyhow::ensure!(first <= last, "Chapter range cannot regress");
            let previous: Option<(i64, i64)> = tx
                .query_row(
                    "SELECT ordinal,last_message_seq FROM context_chapters WHERE session_id=?1 ORDER BY ordinal DESC LIMIT 1",
                    [&chapter.session_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let (previous_ordinal, previous_last) = match previous {
                Some(previous) => previous,
                None => {
                    let cutoff: Option<i64> = tx
                        .query_row(
                            "SELECT through_message_seq FROM context_checkpoints WHERE session_id=?1",
                            [&chapter.session_id],
                            |row| row.get(0),
                        )
                        .optional()?;
                    (0, cutoff.unwrap_or(0))
                }
            };
            anyhow::ensure!(
                previous_ordinal.checked_add(1) == Some(ordinal),
                "Chapter ordinal must follow the previous chapter"
            );
            let next: Option<i64> = tx.query_row(
                "SELECT MIN(seq) FROM messages WHERE session_id=?1 AND seq>?2",
                params![chapter.session_id, previous_last],
                |row| row.get(0),
            )?;
            anyhow::ensure!(
                next == Some(first),
                "Chapter range must start at the next session message after the previous chapter or legacy cutoff"
            );
            tx.execute(
                "INSERT INTO context_chapters(session_id,id,ordinal,first_message_seq,last_message_seq,data) VALUES(?1,?2,?3,?4,?5,?6)",
                params![chapter.session_id, chapter.id, ordinal, first, last, data],
            )?;
            Ok(())
        })
    }

    /// Replace the session's checkpoint atomically, without deleting or copying history.
    pub fn save_context_checkpoint(&self, checkpoint: &ContextCheckpoint) -> Result<()> {
        if let Some(store) = self.scoped(&checkpoint.session_id)? {
            return store.save_context_checkpoint(checkpoint);
        }
        anyhow::ensure!(!checkpoint.id.trim().is_empty(), "Checkpoint ID is empty");
        anyhow::ensure!(
            !checkpoint.session_id.trim().is_empty(),
            "Checkpoint session ID is empty"
        );
        anyhow::ensure!(
            !checkpoint.through_message_id.trim().is_empty(),
            "Checkpoint cutoff is empty"
        );
        anyhow::ensure!(
            !checkpoint.model.trim().is_empty(),
            "Checkpoint model is empty"
        );
        anyhow::ensure!(
            !checkpoint.summary.trim().is_empty() && checkpoint.summary.len() <= 64 * 1024,
            "Checkpoint summary must be nonempty and at most 65536 bytes"
        );
        self.write_context(|tx| {
            let cutoff: Option<i64> = tx.query_row(
                "SELECT seq FROM messages WHERE session_id=?1 AND message_id=?2",
                params![checkpoint.session_id, checkpoint.through_message_id],
                |row| row.get(0),
            ).optional()?;
            let cutoff = cutoff.context("Checkpoint cutoff does not belong to the session")?;
            let session = Self::session_from_connection(tx, &checkpoint.session_id)?;
            if session.context_strategy == ContextStrategy::Classical {
                Self::validate_complete_frontier(tx, &checkpoint.session_id, cutoff)?;
            }
            let previous: Option<i64> = tx.query_row(
                "SELECT through_message_seq FROM context_checkpoints WHERE session_id=?1",
                [&checkpoint.session_id],
                |row| row.get(0),
            ).optional()?;
            anyhow::ensure!(previous.is_none_or(|previous| cutoff >= previous), "Checkpoint cutoff cannot regress");
            // Chapters and episodes start after the legacy cutoff, freezing it.
            let chapters: i64 = tx.query_row(
                "SELECT (SELECT COUNT(*) FROM context_chapters WHERE session_id=?1)+(SELECT COUNT(*) FROM context_episodes WHERE session_id=?1)",
                [&checkpoint.session_id],
                |row| row.get(0),
            )?;
            anyhow::ensure!(chapters == 0, "Legacy checkpoint is frozen once context chapters or episodes exist");
            // Preserve the previous immutable summary before replacing the latest pointer.
            Self::archive_current_checkpoint(tx, &checkpoint.session_id)?;
            let hash = Self::archive_checkpoint(tx, checkpoint)?;
            let mut data = serde_json::to_value(checkpoint)?;
            data["summary"] = serde_json::Value::String(String::new());
            data["_summary_blob"] = serde_json::Value::String(hash);
            let data = serde_json::to_string(&data)?;
            tx.execute(
                "INSERT INTO context_checkpoints(session_id,id,through_message_seq,data) VALUES(?1,?2,?3,?4) ON CONFLICT(session_id) DO UPDATE SET id=excluded.id,through_message_seq=excluded.through_message_seq,data=excluded.data",
                params![checkpoint.session_id, checkpoint.id, cutoff, data],
            )?;
            Ok(())
        })
    }
}
