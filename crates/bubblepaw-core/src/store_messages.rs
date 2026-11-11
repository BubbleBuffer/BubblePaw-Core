//! Snapshots, message reads, attachments and batch appends.
use super::*;

impl Store {
    /// Process-local cache invalidation marker, not a durable replay cursor.
    pub fn revision(&self) -> u64 {
        self.0.transactions.load(Ordering::Relaxed)
    }

    pub fn snapshot(
        &self,
        session_id: &str,
        message_limit: usize,
        run_limit: usize,
        pending_limit: usize,
    ) -> Result<SessionSnapshot> {
        if let Some(store) = self.scoped(session_id)? {
            let mut snapshot =
                store.snapshot(session_id, message_limit, run_limit, pending_limit)?;
            let mut runs = store.all_runs(session_id)?;
            snapshot.run_count = runs.len() as u64;
            if runs.len() > run_limit {
                runs.drain(..runs.len() - run_limit);
            }
            snapshot.runs = runs;
            return Ok(snapshot);
        }
        let conn = self.0.connection.lock().unwrap();
        let session: String = conn.query_row(
            "SELECT data FROM sessions WHERE id=?1",
            [session_id],
            |row| row.get(0),
        )?;
        let read_json = |table: &str, order: &str, limit: usize| -> Result<Vec<String>> {
            let mut statement = conn.prepare(&format!(
                "SELECT data FROM {table} WHERE session_id=?1 ORDER BY {order} DESC LIMIT ?2"
            ))?;
            let mut records = statement
                .query_map(
                    params![session_id, limit.min(i64::MAX as usize) as i64],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            records.reverse();
            Ok(records)
        };
        let messages = read_json("messages", "seq", message_limit)?
            .into_iter()
            .map(|value| Self::message_from_data(&conn, &value))
            .collect::<Result<_>>()?;
        let runs = read_json("runs", "rowid", run_limit)?
            .into_iter()
            .map(|value| Self::run_from_data(&conn, &value))
            .collect::<Result<_>>()?;
        let mut statement = conn.prepare(
            "SELECT id,kind,text,state,origin,content_blob,content_format FROM inputs WHERE session_id=?1 AND state='pending' ORDER BY seq LIMIT ?2"
        )?;
        let pending_inputs = statement
            .query_map(
                params![session_id, pending_limit.min(i64::MAX as usize) as i64],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                },
            )?
            .map(|row| {
                let (id, kind, text, state, origin, content_blob, content_format) = row?;
                let text = Self::input_text(
                    &conn,
                    &text,
                    content_blob.as_deref(),
                    content_format.as_deref(),
                )?;
                Ok(QueuedInput {
                    id,
                    session_id: session_id.into(),
                    kind: serde_json::from_value(serde_json::Value::String(kind))?,
                    text,
                    state,
                    origin: origin
                        .map(|value| serde_json::from_str(&value))
                        .transpose()?,
                })
            })
            .collect::<Result<_>>()?;
        let count = |table: &str, extra: &str| -> Result<u64> {
            Ok(conn.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE session_id=?1 {extra}"),
                [session_id],
                |row| row.get::<_, i64>(0),
            )? as u64)
        };
        Ok(SessionSnapshot {
            revision: self.revision(),
            session: serde_json::from_str(&session)?,
            messages,
            runs,
            pending_inputs,
            message_count: count("messages", "")?,
            run_count: count("runs", "")?,
            pending_input_count: count("inputs", "AND state='pending'")?,
        })
    }

    /// Bounded session metadata without loading full conversations.
    pub fn session_summaries(&self, limit: usize) -> Result<Vec<SessionRecord>> {
        self.session_summaries_impl(limit, false)
    }
    pub(super) fn session_summaries_impl(
        &self,
        limit: usize,
        visible_only: bool,
    ) -> Result<Vec<SessionRecord>> {
        if self.0.layout.is_some() {
            return self.layout_summaries(limit, visible_only);
        }
        let mut records = self
            .list_sessions()?
            .into_iter()
            .filter(|session| !visible_only || !session.hidden)
            .map(|session| {
                let snapshot = self.snapshot(&session.id, 1, 1, 0)?;
                Ok(SessionRecord {
                    session,
                    message_count: snapshot.message_count,
                    last_message: snapshot.messages.into_iter().next(),
                    latest_run: snapshot.runs.into_iter().next(),
                    pending_input_count: snapshot.pending_input_count,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let activity = |record: &SessionRecord| {
            let mut timestamp = record.session.updated_at.clone();
            if let Some(message) = &record.last_message {
                timestamp = timestamp.max(message.created_at.clone());
            }
            if let Some(run) = &record.latest_run {
                timestamp = timestamp.max(run.ended_at.as_ref().unwrap_or(&run.started_at).clone());
            }
            timestamp
        };
        records.sort_by_cached_key(|record| std::cmp::Reverse(activity(record)));
        records.truncate(limit);
        Ok(records)
    }
    pub fn messages(&self, session_id: &str) -> Result<Vec<Message>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.messages(session_id);
        }
        let conn = self.0.connection.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT data FROM messages WHERE session_id=?1 ORDER BY seq")?;
        stmt.query_map([session_id], |r| r.get::<_, String>(0))?
            .map(|r| Self::message_from_data(&conn, &r?))
            .collect()
    }

    /// Read only newly appended records, using the session/sequence index.
    pub fn messages_after(&self, session_id: &str, after_seq: i64) -> Result<Vec<(i64, Message)>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.messages_after(session_id, after_seq);
        }
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT seq,data FROM messages WHERE session_id=?1 AND seq>?2 ORDER BY seq",
            )?;
            stmt.query_map(params![session_id, after_seq], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (seq, data) = row?;
                Ok((seq, Self::message_from_data(conn, &data)?))
            })
            .collect()
        })
    }

    /// Preserve attachment metadata and original bytes in the canonical record.
    pub fn store_attachment(
        &self,
        session_id: &str,
        attachment_id: &str,
        metadata: &str,
        bytes: &[u8],
    ) -> Result<String> {
        if let Some(store) = self.scoped(session_id)? {
            return store.store_attachment(session_id, attachment_id, metadata, bytes);
        }
        anyhow::ensure!(
            !attachment_id.starts_with(CHECKPOINT_PREFIX),
            "Checkpoint IDs are records of write_checkpoint, not attachments"
        );
        self.write(|tx| {
            let hash = Self::blob_put(tx, bytes)?;
            Self::insert_attachment(tx, session_id, attachment_id, metadata, &hash)?;
            Ok(hash)
        })
    }

    /// Insert one attachment row that names `hash`. An existing row must match
    /// exactly, since attachment originals are immutable.
    fn insert_attachment(
        conn: &Connection,
        session_id: &str,
        attachment_id: &str,
        metadata: &str,
        hash: &str,
    ) -> Result<()> {
        conn.execute("INSERT INTO attachments(session_id,id,metadata,blob_hash) VALUES(?1,?2,?3,?4) ON CONFLICT(session_id,id) DO NOTHING", params![session_id, attachment_id, metadata, hash])?;
        let existing: (String, String) = conn.query_row(
            "SELECT metadata,blob_hash FROM attachments WHERE session_id=?1 AND id=?2",
            params![session_id, attachment_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        anyhow::ensure!(
            existing == (metadata.to_owned(), hash.to_owned()),
            "Attachment originals are immutable"
        );
        Ok(())
    }

    /// Session IDs of the chat that holds `session_id`, in order.
    pub fn chat_sessions(&self, session_id: &str) -> Result<Vec<String>> {
        match self.scoped(session_id)? {
            Some(chat) => chat.chat_session_ids(),
            None => self.chat_session_ids(),
        }
    }

    /// Session IDs of this chat store, in order. Only a chat store has them.
    pub fn chat_session_ids(&self) -> Result<Vec<String>> {
        anyhow::ensure!(
            self.0.layout.is_none(),
            "Only a chat store lists its sessions"
        );
        self.read(|conn| {
            let mut stmt = conn.prepare("SELECT id FROM sessions ORDER BY id")?;
            Ok(stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn attachment(
        &self,
        session_id: &str,
        attachment_id: &str,
    ) -> Result<Option<(String, Vec<u8>)>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.attachment(session_id, attachment_id);
        }
        self.read(|conn| {
            let row: Option<(String, String)> = conn
                .query_row(
                    "SELECT metadata,blob_hash FROM attachments WHERE session_id=?1 AND id=?2",
                    params![session_id, attachment_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            row.map(|(metadata, hash)| Ok((metadata, Self::blob_get(conn, &hash)?)))
                .transpose()
        })
    }

    /// Import an already parsed history in one application transaction.
    pub fn append_batch(&self, messages: &[Message]) -> Result<()> {
        if self.0.layout.is_some() {
            return self.layout_append_batch(messages);
        }
        if messages.is_empty() {
            return Ok(());
        }
        self.write(|tx| {
            for message in messages {
                Self::insert_message(tx, message)?;
            }
            Ok(())
        })
    }

    /// Reclaim the idle WAL while retaining FULL synchronization for writes.
    pub fn idle_checkpoint(&self) -> Result<()> {
        if self.0.layout.is_some() {
            return self.layout_checkpoint();
        }
        self.read(|conn| {
            let (busy, _, _): (i64, i64, i64) =
                conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
            anyhow::ensure!(busy == 0, "WAL checkpoint is busy");
            Ok(())
        })
    }
}
