//! Context telemetry, batch appends, forks and housekeeping.
use super::*;

impl Store {
    pub(in crate::store) fn split_context_telemetry(
        &self,
        conn: &Connection,
    ) -> Result<Vec<(String, String, String, String)>> {
        if self.host().is_none() {
            return Ok(Vec::new());
        }
        let mut telemetry = Vec::new();
        for table in [
            "context_checkpoints",
            "context_chapters",
            "context_episodes",
            "context_subject_chapters",
            "context_decisions",
        ] {
            let mut stmt = conn.prepare(&format!("SELECT id,session_id,data FROM {table} WHERE json_type(data,'$.usage') NOT IN ('null') OR json_type(data,'$.reasoning') NOT IN ('null')"))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (key, session, data) in rows {
                let mut value: serde_json::Value = serde_json::from_str(&data)?;
                let usage = value.as_object_mut().unwrap().remove("usage");
                let reasoning = value.as_object_mut().unwrap().remove("reasoning");
                let metadata =
                    serde_json::json!({"record_id":key,"usage":usage,"reasoning":reasoning});
                telemetry.push((
                    session,
                    value["created_at"].as_str().unwrap_or(&now()).to_owned(),
                    table.to_owned(),
                    metadata.to_string(),
                ));
                conn.execute(
                    &format!("UPDATE {table} SET data=?2 WHERE id=?1"),
                    params![key, value.to_string()],
                )?;
            }
        }
        Ok(telemetry)
    }
    pub(in crate::store) fn save_telemetry_best_effort(
        &self,
        rows: &[(String, String, String, String)],
    ) {
        if rows.is_empty() {
            return;
        }
        if let Some(host) = self.host() {
            let _ = host.write(|tx| {
                for (session, created, kind, data) in rows {
                    let key = serde_json::from_str::<serde_json::Value>(data)?["record_id"].as_str().context("Telemetry record ID missing")?.to_owned();
                    tx.execute("DELETE FROM telemetry WHERE session_id=?1 AND kind=?2 AND json_extract(data,'$.record_id')=?3", params![session, kind, key])?;
                    tx.execute("INSERT INTO telemetry(session_id,created_at,kind,data) VALUES(?1,?2,?3,?4)", params![session, created, kind, data])?;
                }
                Ok(())
            });
        }
    }
    pub(in crate::store) fn hydrate_context_telemetry(&self, data: &str) -> Result<String> {
        let Some(host) = self.host() else {
            return Ok(data.into());
        };
        let mut value: serde_json::Value = serde_json::from_str(data)?;
        let key = value["id"].as_str().context("Context record ID missing")?;
        let session = value["session_id"]
            .as_str()
            .context("Context session ID missing")?;
        let metadata: Option<String> = host.read(|conn| Ok(conn.query_row("SELECT data FROM telemetry WHERE session_id=?1 AND json_extract(data,'$.record_id')=?2 ORDER BY id DESC LIMIT 1", params![session, key], |row| row.get(0)).optional()?))?;
        if let Some(metadata) = metadata {
            let metadata: serde_json::Value = serde_json::from_str(&metadata)?;
            for field in ["usage", "reasoning"] {
                if !metadata[field].is_null() {
                    value[field] = metadata[field].clone();
                }
            }
        }
        Ok(value.to_string())
    }
    pub(crate) fn all_runs(&self, session: &str) -> Result<Vec<Run>> {
        let mut rows = self.read(|conn| {
            let mut stmt =
                conn.prepare("SELECT data FROM runs WHERE session_id=?1 ORDER BY rowid")?;
            Ok(stmt
                .query_map([session], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        if let Some(host) = self.host() {
            rows.extend(host.read(|conn| {
                let mut stmt =
                    conn.prepare("SELECT data FROM runs WHERE session_id=?1 ORDER BY rowid")?;
                Ok(stmt
                    .query_map([session], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })?);
        }
        let mut runs = self.read(|conn| {
            rows.into_iter()
                .map(|data| Self::run_from_data(conn, &data))
                .collect::<Result<Vec<_>>>()
        })?;
        runs.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.id.cmp(&b.id)));
        runs.dedup_by(|a, b| a.id == b.id);
        Ok(runs)
    }
    pub(crate) fn input_event_row(&self, session: &str, input: &str) -> Result<InputEventRow> {
        let sql = "SELECT text,message_id,content_blob,content_format FROM inputs WHERE session_id=?1 AND id=?2";
        let row = self.read(|conn| {
            Ok(conn
                .query_row(sql, params![session, input], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .optional()?)
        })?;
        if let Some(row) = row {
            return Ok(row);
        }
        let host = self.host().context("Unknown accepted input")?;
        host.read(|conn| {
            Ok(conn.query_row(sql, params![session, input], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?)
        })
    }
    pub(in crate::store) fn layout_append_batch(&self, messages: &[Message]) -> Result<()> {
        let mut groups: HashMap<PathBuf, (Self, Vec<Message>)> = HashMap::new();
        for message in messages {
            let chat = self.scoped(&message.session_id)?.unwrap();
            groups
                .entry(chat.0.path.clone().unwrap())
                .or_insert_with(|| (chat, Vec::new()))
                .1
                .push(message.clone());
        }
        // Each group is its own authoritative commit; no cross-file transaction.
        for (chat, messages) in groups.into_values() {
            chat.append_batch(&messages)?;
        }
        Ok(())
    }
    pub(in crate::store) fn layout_fork(
        &self,
        source_id: &str,
        message_id: Option<&str>,
        title: String,
        admitted: impl FnOnce() -> Result<()>,
        after: impl FnOnce(&Session) -> Result<()>,
    ) -> Result<Session> {
        let runtime = self.runtime();
        let _guard = runtime.fork_guard(source_id)?;
        admitted()?;
        let source = self.scoped(source_id)?.unwrap();
        let dispatched = source.read(|conn| {
            Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM effects WHERE session_id=?1 AND state='dispatched')",
                [source_id],
                |row| row.get::<_, bool>(0),
            )?)
        })?;
        anyhow::ensure!(!dispatched, "Cannot fork with active background effects");
        let history = Self::fork_history(source.messages(source_id)?, message_id)?;
        let mut session = self.create_session_with_parent(
            title,
            source.session(source_id)?.context_strategy,
            Some(source_id),
        )?;
        session.parent_cutoff = Some(history.len() as i64);
        source.write(|tx| {
            tx.execute(
                "UPDATE sessions SET data=?2 WHERE id=?1",
                params![session.id, serde_json::to_string(&session)?],
            )?;
            for mut message in history {
                let original_message_id = message.id.clone();
                message.id = id();
                message.session_id = session.id.clone();
                Self::insert_message(tx, &message)?;
                Self::copy_reminder_metadata_tx(
                    tx,
                    source_id,
                    &original_message_id,
                    &session.id,
                    &message.id,
                )?;
            }
            Ok(())
        })?;
        after(&session)?;
        Ok(session)
    }
    pub fn serving_wipe(&self) -> Result<()> {
        anyhow::ensure!(self.is_layout(), "Serving wipe requires a directory store");
        self.runtime().log.reset_serving(|last| {
            let path = self.0.path.as_ref().unwrap();
            let mut connection = self.0.connection.lock().unwrap();
            // Drop the old connection before removing files (also on Windows).
            let old = std::mem::replace(&mut *connection, Connection::open_in_memory()?);
            drop(old);
            for suffix in ["", "-wal", "-shm"] {
                let file = PathBuf::from(format!("{}{suffix}", path.display()));
                if file.exists() {
                    std::fs::remove_file(file)?;
                }
            }
            *connection = projection(
                path,
                0x42504232,
                SERVING_SCHEMA,
                "serving",
                schema::SERVING_VERSION,
            )?;
            let tx = connection.transaction()?;
            tx.execute(
                "INSERT INTO metadata VALUES('events_last_seq',?1)",
                [last.to_string()],
            )?;
            tx.execute(
                "INSERT INTO metadata VALUES('events_pruned_through',?1)",
                [last.to_string()],
            )?;
            tx.commit()?;
            Ok(())
        })
    }
    pub fn housekeeping(&self) -> Result<()> {
        let Some(layout) = &self.0.layout else {
            return Ok(());
        };
        let instant = chrono::Utc::now();
        let host_telemetry_cutoff = instant
            .checked_sub_signed(chrono::Duration::days(i64::from(
                layout.options.telemetry_retention_days,
            )))
            .context("Telemetry retention is outside the supported date range")?
            .to_rfc3339();
        let cutoffs = {
            let catalog = layout.catalog.lock().unwrap();
            let mut statement = catalog.prepare("SELECT s.id,m.value FROM sessions s LEFT JOIN metadata m ON m.key='root_storage:'||s.root_id")?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .map(|row| -> Result<_> {
                    let (session, value) = row?;
                    let policy = value
                        .map(|value| serde_json::from_str::<Option<RootStoragePolicy>>(&value))
                        .transpose()?
                        .flatten();
                    let options = policy.map_or(layout.options, |policy| policy.store);
                    let events = instant
                        .checked_sub_signed(chrono::Duration::hours(i64::from(
                            options.finished_event_retention_hours,
                        )))
                        .context("Finished event retention is outside the supported date range")?
                        .to_rfc3339();
                    let telemetry = instant
                        .checked_sub_signed(chrono::Duration::days(i64::from(
                            options.telemetry_retention_days,
                        )))
                        .context("Telemetry retention is outside the supported date range")?
                        .to_rfc3339();
                    Ok((session, events, telemetry))
                })
                .collect::<Result<Vec<_>>>()?
        };
        let pruned = self.write(|tx| {
            // Cutoffs are a transient serving view. All roots are pruned in one
            // serving transaction, and canonical chats remain untouched here.
            tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS root_housekeeping(session_id TEXT PRIMARY KEY,event_cutoff TEXT NOT NULL,telemetry_cutoff TEXT NOT NULL) WITHOUT ROWID; DELETE FROM root_housekeeping;")?;
            {
                let mut insert = tx.prepare_cached("INSERT INTO root_housekeeping VALUES(?1,?2,?3)")?;
                for (session, events, telemetry) in &cutoffs {
                    insert.execute(params![session, events, telemetry])?;
                }
            }
            let through: Option<i64> = tx.query_row("SELECT MAX(e.seq) FROM events e JOIN root_housekeeping p ON p.session_id=e.session_id JOIN runs r ON r.id=e.run_id WHERE e.created_at<p.event_cutoff AND r.status NOT IN ('running','waiting_for_quota') AND COALESCE(json_extract(r.data,'$.ended_at'),'')<p.event_cutoff", [], |row| row.get(0))?;
            let previous: Option<String> = tx.query_row("SELECT value FROM metadata WHERE key='events_pruned_through'", [], |row| row.get(0)).optional()?;
            let through = through.map(|through| through.max(previous.and_then(|value| value.parse().ok()).unwrap_or(0)));
            if let Some(through) = through {
                // Retain active-run events even inside the pruned range. A client
                // behind its highest removed sequence must take a new snapshot.
                tx.execute("DELETE FROM events WHERE seq<=?1 AND EXISTS(SELECT 1 FROM root_housekeeping p JOIN runs r ON r.id=events.run_id WHERE p.session_id=events.session_id AND events.created_at<p.event_cutoff AND r.status NOT IN ('running','waiting_for_quota') AND COALESCE(json_extract(r.data,'$.ended_at'),'')<p.event_cutoff)", [through])?;
                tx.execute("INSERT OR REPLACE INTO metadata VALUES('events_pruned_through',?1)", [through.to_string()])?;
            }
            // Finished run metadata is telemetry (including usage/timings), so
            // keep it for the telemetry window after its event journal expires.
            tx.execute("DELETE FROM runs WHERE status NOT IN ('running','waiting_for_quota') AND EXISTS(SELECT 1 FROM root_housekeeping p WHERE p.session_id=runs.session_id AND COALESCE(json_extract(runs.data,'$.ended_at'),'')<p.telemetry_cutoff) AND NOT EXISTS(SELECT 1 FROM events WHERE events.run_id=runs.id)", [])?;
            tx.execute("DELETE FROM approvals WHERE state!='pending' AND EXISTS(SELECT 1 FROM root_housekeeping p WHERE p.session_id=approvals.session_id AND approvals.decided_at<p.event_cutoff) AND NOT EXISTS(SELECT 1 FROM events WHERE events.run_id=approvals.run_id)", [])?;
            tx.execute("DELETE FROM inputs WHERE state NOT IN ('pending','reminder_pending') AND EXISTS(SELECT 1 FROM root_housekeeping p WHERE p.session_id=inputs.session_id AND inputs.resolved_at<p.event_cutoff) AND NOT EXISTS(SELECT 1 FROM events WHERE json_extract(events.data,'$._content_ref.input_id')=inputs.id)", [])?;
            // Host-only telemetry has no root policy; preserve its legacy bound.
            super::usage::prompt_log::prune(tx, &host_telemetry_cutoff)?;
            Ok(through)
        })?;
        if let Some(pruned) = pruned {
            let last = self.runtime().log.last();
            self.runtime().log.seed(last, pruned as u64);
        }
        let chats: Vec<_> = layout.chats.lock().unwrap().values().cloned().collect();
        for chat in chats {
            for session in chat.list_sessions()? {
                chat.compress_cold_messages(&session.id)?;
            }
        }
        self.flush_serving()?;
        self.archive_idle_chats()?;
        Ok(())
    }
}
