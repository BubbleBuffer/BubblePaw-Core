//! Catalog rebuild and session activity records.
use super::*;

impl Store {
    pub fn catalog_rebuild(&self) -> Result<usize> {
        self.catalog_rebuild_mode(true)
    }
    pub(super) fn catalog_rebuild_mode(&self, force_archived: bool) -> Result<usize> {
        // Standalone catalog repair must not compete with an attached buffered
        // projection transaction or leave that transaction on an old snapshot.
        self.flush_serving()?;
        let layout = self
            .0
            .layout
            .as_ref()
            .context("Catalog requires a directory store")?;
        self.recover_archives()?;
        let rows_at_start = Self::catalog_ids(layout)?;
        let mut records = Vec::new();
        let mut search_targets: HashMap<String, i64> = {
            let catalog = layout.catalog.lock().unwrap();
            let mut statement = catalog.prepare(
                "SELECT substr(key,15),value FROM metadata WHERE substr(key,1,14)='search_target:'",
            )?;
            let mut targets = HashMap::new();
            for row in statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })? {
                let (session, target) = row?;
                if let Ok(target) = target.parse::<i64>() {
                    targets.insert(session, target);
                }
            }
            targets
        };
        // The catalog workspace is only the legacy fallback. Root chats may
        // carry different authoritative bindings; rebuilding never selects one
        // of those roots as the default for the others.
        self.bound_workspace()?;
        let mut archive_times: HashMap<String, String> = HashMap::new();
        let mut known_archives: HashMap<String, Vec<(Session, String)>> = HashMap::new();
        if !force_archived {
            let catalog = layout.catalog.lock().unwrap();
            // A completed archive is immutable, and its complete session tree
            // was catalogued before the active -> archiving transition. Normal
            // startup keeps those metadata rows without decoding cold files.
            let mut statement=catalog.prepare("SELECT root_id,data,last_activity FROM sessions AS archived WHERE storage_state='archived' AND EXISTS(SELECT 1 FROM sessions AS root WHERE root.id=archived.root_id AND root.storage_state='archived') AND NOT EXISTS(SELECT 1 FROM sessions AS mixed WHERE mixed.root_id=archived.root_id AND mixed.storage_state!='archived')")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let root: String = row.get(0)?;
                let session: Session = serde_json::from_str(&row.get::<_, String>(1)?)?;
                known_archives
                    .entry(root)
                    .or_default()
                    .push((session, row.get(2)?));
            }
        }
        let mut archived_roots = std::collections::HashSet::new();
        let mut root_storage: HashMap<String, Option<RootStoragePolicy>> = HashMap::new();
        for entry in std::fs::read_dir(layout.data_dir.join("chats"))? {
            let mut path = entry?.path();
            let archived = path.to_string_lossy().ends_with(".db.zst");
            if archived {
                path = path.with_extension("");
                if path.is_file() {
                    continue;
                }
            } else if path.extension().is_none_or(|extension| extension != "db") {
                continue;
            }
            let root = path
                .file_stem()
                .context("Chat filename missing")?
                .to_string_lossy()
                .into_owned();
            if archived && let Some(sessions) = known_archives.remove(&root) {
                let projected: Option<String> = layout
                    .catalog
                    .lock()
                    .unwrap()
                    .query_row(
                        "SELECT value FROM metadata WHERE key=?1",
                        [format!("root_storage:{root}")],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(value) = projected
                    && let Ok(policy) = serde_json::from_str::<Option<RootStoragePolicy>>(&value)
                    && policy
                        .as_ref()
                        .is_none_or(|policy| policy.validate().is_ok())
                {
                    root_storage.insert(root.clone(), policy);
                    archived_roots.insert(root.clone());
                    records.extend(
                        sessions
                            .into_iter()
                            .map(|(session, activity)| (root.clone(), session, activity)),
                    );
                    continue;
                }
            }
            // A malformed workspace binding rejects only its own chat: the
            // rebuild skips it with a notice and never rewrites the file.
            let binding_error = |conn: &Connection| -> Result<Option<String>> {
                let mut bindings = conn.prepare(
                    "SELECT key,value FROM metadata WHERE key='workspace' OR substr(key,1,15)='root_workspace:'"
                )?;
                for binding in bindings.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })? {
                    let (key, value) = binding?;
                    if key != "workspace" && key != format!("root_workspace:{root}") {
                        return Ok(Some(format!("binding key {key} names another root")));
                    }
                    if let Err(error) = Self::workspace_binding(&value) {
                        return Ok(Some(format!("{error:#}")));
                    }
                }
                Ok(None)
            };
            let skip = |reason: String| {
                tracing::warn!(
                    "Skipping chat {root}: Malformed root workspace binding ({reason}); the chat file is left unchanged"
                );
            };
            let inspect = |conn: &Connection| -> Result<(Vec<Session>, Option<RootStoragePolicy>)> {
                let mut statement = conn.prepare("SELECT data FROM sessions ORDER BY rowid")?;
                let sessions = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .map(|row| Ok(serde_json::from_str(&row?)?))
                    .collect::<Result<Vec<Session>>>()?;
                let mut storage = conn.prepare(
                    "SELECT key,value FROM metadata WHERE substr(key,1,13)='root_storage:'",
                )?;
                let mut policy = None;
                for row in storage.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })? {
                    let (key, value) = row?;
                    anyhow::ensure!(
                        key == format!("root_storage:{root}"),
                        "Malformed root storage policy binding"
                    );
                    let saved = serde_json::from_str::<RootStoragePolicy>(&value)
                        .context("Malformed root storage policy")?;
                    saved.validate()?;
                    policy = Some(saved);
                }
                Ok((sessions, policy))
            };
            let mut archived_record = None;
            let mut active_record = None;
            let (sessions, policy) = if archived {
                let record = open_record(&path, true, true)?;
                if let Some(reason) = binding_error(&record)? {
                    skip(reason);
                    continue;
                }
                archived_roots.insert(root.clone());
                archived_record = Some(record);
                if let Some(time) = archived_record
                    .as_ref()
                    .unwrap()
                    .query_row(
                        "SELECT value FROM metadata WHERE key='archived_at'",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                {
                    archive_times.insert(root.clone(), time);
                }
                inspect(archived_record.as_ref().unwrap())?
            } else {
                let record = self.open_chat(&root)?;
                if let Some(reason) = record.read(binding_error)? {
                    skip(reason);
                    layout.chats.lock().unwrap().remove(&root);
                    continue;
                }
                active_record = Some(record);
                active_record.as_ref().unwrap().read(inspect)?
            };
            root_storage.insert(root.clone(), policy);
            for session in sessions {
                let target = |conn: &Connection| -> Result<i64> {
                    Ok(conn.query_row(
                        "SELECT COALESCE(MAX(seq),0) FROM messages WHERE session_id=?1",
                        [&session.id],
                        |row| row.get(0),
                    )?)
                };
                let maximum = if archived {
                    target(archived_record.as_ref().unwrap())?
                } else {
                    active_record.as_ref().unwrap().read(target)?
                };
                search_targets.insert(session.id.clone(), maximum);
                let activity = if archived {
                    Self::record_activity_connection(archived_record.as_ref().unwrap(), &session)?
                } else {
                    active_record.as_ref().unwrap().record_activity(&session)?
                };
                records.push((root.clone(), session, activity));
            }
        }
        let newer = Self::swap_catalog(
            layout,
            &rows_at_start,
            &records,
            &search_targets,
            catalog_swap::ScannedRoots {
                storage: root_storage,
                archive_times,
                archived: &archived_roots,
            },
        )?;
        records.extend(newer);
        // The rebuilt catalog is authoritative again; drop every resolved placement.
        layout.session_roots.lock().unwrap().clear();
        // Session metadata in serving supports compatibility without owning content.
        let removed_events = self.write(|tx| {
            tx.execute("DELETE FROM sessions", [])?;
            for (_, session, _) in &records {
                tx.execute(
                    "INSERT INTO sessions VALUES(?1,?2)",
                    params![session.id, serde_json::to_string(session)?],
                )?;
                if let Some(target) = search_targets.get(&session.id) {
                    // Do this before later appends can advance a previously
                    // complete watermark past an index tail lost on interruption.
                    tx.execute("UPDATE search_progress SET complete=0 WHERE session_id=?1 AND through_seq<?2", params![session.id,target])?;
                }
            }
            let orphaned_search: Vec<String> = {
                let mut statement=tx.prepare("SELECT DISTINCT session_id FROM search_rows WHERE NOT EXISTS(SELECT 1 FROM sessions WHERE sessions.id=search_rows.session_id)")?;
                statement.query_map([],|row|row.get(0))?.collect::<rusqlite::Result<_>>()?
            };
            Self::remove_search_sessions(tx,&orphaned_search)?;
            let removed: Option<i64> = tx.query_row("SELECT MAX(seq) FROM events WHERE NOT EXISTS(SELECT 1 FROM sessions WHERE sessions.id=events.session_id)", [], |row| row.get(0))?;
            for table in ["events", "context_views", "runs", "inputs", "approvals", "telemetry"] {
                tx.execute(&format!("DELETE FROM {table} WHERE session_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM sessions WHERE sessions.id={table}.session_id)"), [])?;
            }
            if let Some(removed) = removed {
                crate::event_log::remember_last(tx, removed as u64)?;
                let previous: Option<String> = tx.query_row("SELECT value FROM metadata WHERE key='events_pruned_through'", [], |row| row.get(0)).optional()?;
                let pruned = removed.max(previous.and_then(|value| value.parse().ok()).unwrap_or(0));
                tx.execute("INSERT OR REPLACE INTO metadata VALUES('events_pruned_through',?1)", [pruned.to_string()])?;
                return Ok(Some(pruned as u64));
            }
            Ok(None)
        })?;
        if let Some(removed) = removed_events {
            self.runtime().log.mark_pruned(removed);
        }
        Ok(records.len())
    }
    fn record_activity(&self, session: &Session) -> Result<String> {
        self.read(|conn| Self::record_activity_connection(conn, session))
    }
    fn record_activity_connection(conn: &Connection, session: &Session) -> Result<String> {
        let mut activity = session.updated_at.clone();
        // Activity is metadata: a large tool body must not be decompressed again
        // after every record step just to obtain its timestamp.
        let created: Option<String> = conn.query_row("SELECT created_at FROM message_headers WHERE session_id=?1 ORDER BY message_seq DESC LIMIT 1", [&session.id], |row| row.get(0)).optional()?;
        if let Some(created) = created {
            activity = activity.max(created);
        }
        let run: Option<String> = conn
            .query_row(
                "SELECT data FROM runs WHERE session_id=?1 ORDER BY rowid DESC LIMIT 1",
                [&session.id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(run) = run {
            let run: serde_json::Value = serde_json::from_str(&run)?;
            if let Some(created) = run["ended_at"]
                .as_str()
                .or_else(|| run["started_at"].as_str())
            {
                activity = activity.max(created.to_owned());
            }
        }
        Ok(activity)
    }
    pub(in crate::store) fn catalog_session(
        conn: &Connection,
        root: &str,
        session: &Session,
        activity: &str,
    ) -> Result<()> {
        Self::catalog_session_to(conn, "sessions", root, session, activity)
    }
    pub(in crate::store) fn catalog_session_to(
        conn: &Connection,
        table: &str,
        root: &str,
        session: &Session,
        activity: &str,
    ) -> Result<()> {
        anyhow::ensure!(
            matches!(table, "sessions" | "catalog_projection.sessions"),
            "Invalid catalog destination"
        );
        let sql = format!(
            "INSERT INTO {table}(id,parent_id,root_id,title,last_activity,created_at,file,storage_state,data) VALUES(?1,?2,?3,?4,?5,?6,?7,'active',?8) ON CONFLICT(id) DO UPDATE SET parent_id=excluded.parent_id,root_id=excluded.root_id,title=excluded.title,last_activity=excluded.last_activity,created_at=excluded.created_at,file=excluded.file,storage_state='active',data=excluded.data WHERE sessions.data!=excluded.data OR sessions.last_activity!=excluded.last_activity"
        );
        conn.execute(
            &sql,
            params![
                session.id,
                session.parent_session_id,
                root,
                session.title,
                activity,
                session.created_at,
                format!("chats/{root}.db"),
                serde_json::to_string(session)?
            ],
        )?;
        Ok(())
    }
    pub(in crate::store) fn refresh_catalog_best_effort(&self) {
        let Some(host) = self.host() else {
            return;
        };
        let Some(layout) = &host.0.layout else {
            return;
        };
        let Some(root) = self
            .0
            .path
            .as_ref()
            .and_then(|path| path.file_stem())
            .map(|root| root.to_string_lossy().into_owned())
        else {
            return;
        };
        let result = (|| -> Result<()> {
            let sessions = self.list_sessions()?;
            let records = sessions
                .iter()
                .map(|session| Ok((session, self.record_activity(session)?)))
                .collect::<Result<Vec<_>>>()?;
            let mut pending = layout.pending_catalog.lock().unwrap();
            for (session, activity) in records {
                pending.insert(
                    session.id.clone(),
                    (root.clone(), session.clone(), activity),
                );
            }
            Ok(())
        })();
        // Catalog is explicitly rebuildable; it never changes success of record commits.
        let _ = result;
    }
}
