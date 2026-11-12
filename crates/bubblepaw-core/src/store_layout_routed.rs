//! Layout-routed session reads, summaries and record lookups.
use super::*;

impl Store {
    pub(in crate::store) fn layout_bind_workspace(&self, path: &Path) -> Result<()> {
        let root = path.canonicalize()?.to_string_lossy().into_owned();
        let layout = self.0.layout.as_ref().unwrap();
        let existing: Option<String> = layout
            .catalog
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM metadata WHERE key='workspace'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            anyhow::ensure!(
                existing == root,
                "Database belongs to another workspace: {existing}"
            );
            return Ok(());
        }
        let mut catalog = layout.catalog.lock().unwrap();
        let tx = catalog.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO metadata VALUES('workspace',?1)",
            [&root],
        )?;
        tx.commit()?;
        layout.catalog_transactions.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    pub(in crate::store) fn layout_session_workspace(
        &self,
        session: &str,
    ) -> Result<Option<PathBuf>> {
        let layout = self.0.layout.as_ref().unwrap();
        let workspace = self
            .open_listed_session(session, |root, path| {
                let cached = layout.chats.lock().unwrap().get(root).cloned();
                if let Some(chat) = cached {
                    return chat.read(|conn| {
                        Self::session_workspace_connection(conn, session, Some(root))
                    });
                }
                let record = open_record(path, true, false)?;
                Self::session_workspace_connection(&record, session, Some(root))
            })?
            .context("Unknown session or missing chat file")?
            .0;
        match workspace {
            Some(workspace) => Ok(Some(workspace)),
            None => self.bound_workspace(),
        }
    }
    pub(in crate::store) fn layout_session_storage_policy(
        &self,
        session: &str,
    ) -> Result<Option<RootStoragePolicy>> {
        let layout = self.0.layout.as_ref().unwrap();
        // This immutable projection is rebuilt from root-chat metadata. It also
        // lets retention inspect archived roots without decoding their bodies.
        let projected: Option<String> = layout.catalog.lock().unwrap().query_row(
            "SELECT m.value FROM sessions s LEFT JOIN metadata m ON m.key='root_storage:'||s.root_id WHERE s.id=?1", [session], |row| row.get(0)
        ).optional()?.flatten();
        if let Some(value) = projected
            && let Ok(policy) = serde_json::from_str::<Option<RootStoragePolicy>>(&value)
            && policy
                .as_ref()
                .is_none_or(|policy| policy.validate().is_ok())
        {
            return Ok(policy);
        }
        Ok(self
            .open_listed_session(session, |root, path| {
                let cached = layout.chats.lock().unwrap().get(root).cloned();
                if let Some(chat) = cached {
                    return chat
                        .read(|conn| Self::session_storage_connection(conn, session, Some(root)));
                }
                let record = open_record(path, true, false)?;
                Self::session_storage_connection(&record, session, Some(root))
            })?
            .context("Unknown session or missing chat file")?
            .0)
    }
    pub(in crate::store) fn layout_sessions(&self) -> Result<Vec<Session>> {
        let layout = self.0.layout.as_ref().unwrap();
        let mut sessions: HashMap<String, (Session, String)> = {
            let catalog = layout.catalog.lock().unwrap();
            let mut stmt = catalog.prepare("SELECT data,last_activity FROM sessions")?;
            stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (data, activity) = row?;
                let session: Session = serde_json::from_str(&data)?;
                Ok((session.id.clone(), (session, activity)))
            })
            .collect::<Result<_>>()?
        };
        for (_, session, activity) in layout.pending_catalog.lock().unwrap().values() {
            sessions.insert(session.id.clone(), (session.clone(), activity.clone()));
        }
        let mut sessions: Vec<_> = sessions.into_values().collect();
        sessions.sort_by(|(left, la), (right, ra)| ra.cmp(la).then_with(|| left.id.cmp(&right.id)));
        Ok(sessions.into_iter().map(|(session, _)| session).collect())
    }
    pub(in crate::store) fn layout_summaries(
        &self,
        limit: usize,
        visible_only: bool,
    ) -> Result<Vec<SessionRecord>> {
        // A sidebar read must not reactivate cold chats. Share each temporary
        // read-only unpack across its root and child rows for this bounded page.
        let layout = self.0.layout.as_ref().unwrap();
        let mut archives: HashMap<String, RecordConnection> = HashMap::new();
        self.list_sessions()?
            .into_iter()
            .filter(|session| !visible_only || !session.hidden)
            .take(limit)
            .map(|session| {
                if self.active_session_metadata(&session.id)?.is_none() {
                    let (root, _) = open_session_record(
                        &layout.catalog,
                        &layout.data_dir,
                        &session.id,
                        |root, path| {
                            if let std::collections::hash_map::Entry::Vacant(entry) =
                                archives.entry(root.to_owned())
                            {
                                entry.insert(open_record(path, true, true)?);
                            }
                            Ok(root.to_owned())
                        },
                    )?
                    .context("Summary record was removed")?;
                    return self.cold_session_summary(archives.get(&root).unwrap(), session);
                }
                let snap = self.snapshot(&session.id, 1, 1, 0)?;
                Ok(SessionRecord {
                    session,
                    message_count: snap.message_count,
                    last_message: snap.messages.into_iter().next(),
                    latest_run: snap.runs.into_iter().next(),
                    pending_input_count: snap.pending_input_count,
                })
            })
            .collect()
    }
    /// Read summary fields through the ordinary decoder without changing a
    /// record's placement. Finished run metadata may already live in serving.
    fn cold_session_summary(&self, record: &Connection, session: Session) -> Result<SessionRecord> {
        let count = |table: &str, extra: &str| -> Result<u64> {
            Ok(record.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE session_id=?1 {extra}"),
                [&session.id],
                |row| row.get::<_, i64>(0),
            )? as u64)
        };
        let data: Option<String> = record
            .query_row(
                "SELECT data FROM messages WHERE session_id=?1 ORDER BY seq DESC LIMIT 1",
                [&session.id],
                |row| row.get(0),
            )
            .optional()?;
        let last_message = data
            .map(|data| Self::message_from_data(record, &data))
            .transpose()?;
        let latest = |conn: &Connection| -> Result<Option<String>> {
            Ok(conn.query_row("SELECT data FROM runs WHERE session_id=?1 ORDER BY json_extract(data,'$.started_at') DESC,id DESC LIMIT 1", [&session.id], |row| row.get(0)).optional()?)
        };
        let mut runs = [latest(record)?, self.read(latest)?]
            .into_iter()
            .flatten()
            .map(|data| Self::run_from_data(record, &data))
            .collect::<Result<Vec<_>>>()?;
        runs.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.id.cmp(&b.id)));
        runs.dedup_by(|a, b| a.id == b.id);
        Ok(SessionRecord {
            message_count: count("messages", "")?,
            pending_input_count: count("inputs", "AND state='pending'")?,
            last_message,
            latest_run: runs.pop(),
            session,
        })
    }
    pub(crate) fn find_record(&self, table: &str, key: &str) -> Result<Self> {
        if !self.is_layout() {
            return Ok(self.clone());
        }
        anyhow::ensure!(
            ["runs", "effects", "approvals"].contains(&table),
            "Invalid record table"
        );
        if table != "effects" {
            let session: Option<String> = self.read(|conn| {
                Ok(conn
                    .query_row(
                        &format!("SELECT session_id FROM {table} WHERE id=?1"),
                        [key],
                        |row| row.get(0),
                    )
                    .optional()?)
            })?;
            if let Some(session) = session {
                return Ok(self.scoped(&session)?.unwrap());
            }
        }
        for session in self.list_sessions()? {
            let chat = self.scoped(&session.id)?.unwrap();
            let exists = chat.read(|conn| {
                Ok(conn.query_row(
                    &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE id=?1)"),
                    [key],
                    |row| row.get::<_, bool>(0),
                )?)
            })?;
            if exists {
                return Ok(chat);
            }
        }
        // Archival commits serving first, then removes the canonical operation
        // row. It may have crossed both earlier lookups while this scan ran.
        if table != "effects" {
            let session: Option<String> = self.read(|conn| {
                Ok(conn
                    .query_row(
                        &format!("SELECT session_id FROM {table} WHERE id=?1"),
                        [key],
                        |row| row.get(0),
                    )
                    .optional()?)
            })?;
            if let Some(session) = session {
                return Ok(self.scoped(&session)?.unwrap());
            }
        }
        bail!("Unknown {table} record: {key}")
    }
    pub(crate) fn run_row(&self, run_id: &str) -> Result<(String, Option<String>)> {
        let row = self.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT data,result_message_id FROM runs WHERE id=?1",
                    [run_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?)
        })?;
        if let Some(row) = row {
            return Ok(row);
        }
        if let Some(host) = self.host() {
            return host.read(|conn| {
                Ok(conn.query_row(
                    "SELECT data,result_message_id FROM runs WHERE id=?1",
                    [run_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            });
        }
        bail!("Unknown run: {run_id}")
    }
    pub(crate) fn approval_record(&self, approval: &str) -> Result<crate::ApprovalRecord> {
        if self.is_layout() {
            return self
                .find_record("approvals", approval)?
                .approval_record(approval);
        }
        self.read(|conn| {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM approvals WHERE id=?1)",
                [approval],
                |row| row.get(0),
            )?;
            if exists {
                return crate::approvals::read_record(conn, approval);
            }
            let host = self.host().context("Unknown approval")?;
            host.read(|projection| crate::approvals::read_record_from(conn, projection, approval))
        })
    }
    pub(crate) fn archived_approval_state(&self, approval: &str) -> Result<Option<String>> {
        let host = if self.is_layout() {
            Some(self.clone())
        } else {
            self.host()
        };
        match host {
            Some(host) => host.read(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT state FROM approvals WHERE id=?1",
                        [approval],
                        |row| row.get(0),
                    )
                    .optional()?)
            }),
            None => Ok(None),
        }
    }
    pub(crate) fn archive_operations(&self) -> Result<()> {
        let Some(host) = self.host() else {
            return Ok(());
        };
        let runs: Vec<String> = self.read(|conn| {
            let mut stmt =
                conn.prepare("SELECT id FROM runs WHERE status NOT IN ('running','interrupted','waiting_for_quota')")?;
            Ok(stmt
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        })?;
        let approvals: Vec<String> = self.read(|conn| {
            let mut stmt = conn.prepare("SELECT id FROM approvals WHERE state!='pending'")?;
            Ok(stmt
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        })?;
        let inputs: Vec<String> = self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM inputs WHERE state NOT IN ('pending','reminder_pending')",
            )?;
            Ok(stmt
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        })?;
        if runs.is_empty() && approvals.is_empty() && inputs.is_empty() {
            return Ok(());
        }
        self.read(|record| host.write(|projection| {
            for (table, ids) in [("runs", &runs), ("approvals", &approvals)] {
                for key in ids { Self::copy_rows(record, projection, table, " WHERE id=?", std::slice::from_ref(key))?; }
            }
            for key in &inputs {
                let values = record.query_row("SELECT seq,id,session_id,kind,text,state,origin,message_id,content_blob,content_format FROM inputs WHERE id=?1", [key], |row| (0..10).map(|index| row.get::<_, rusqlite::types::Value>(index)).collect::<rusqlite::Result<Vec<_>>>())?;
                let mut values = values; values.push(now().into());
                projection.execute("INSERT INTO inputs(source_seq,id,session_id,kind,text,state,origin,message_id,content_blob,content_format,resolved_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) ON CONFLICT(id) DO NOTHING", rusqlite::params_from_iter(values))?;
            }
            Ok(())
        }))?;
        self.write(|tx| {
            for key in &runs {
                tx.execute(
                    "DELETE FROM runs WHERE id=?1 AND status NOT IN ('running','interrupted','waiting_for_quota')",
                    [key],
                )?;
            }
            for key in &approvals {
                tx.execute(
                    "DELETE FROM approvals WHERE id=?1 AND state!='pending'",
                    [key],
                )?;
            }
            for key in &inputs {
                tx.execute("DELETE FROM inputs WHERE id=?1 AND state NOT IN ('pending','reminder_pending')", [key])?;
            }
            tx.execute("UPDATE effects SET detail=NULL WHERE state='completed'", [])?;
            Ok(())
        })
    }
}
