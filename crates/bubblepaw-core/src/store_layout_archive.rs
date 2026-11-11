//! Chat state, archive and archive recovery.
use super::*;

impl Store {
    fn set_chat_state(&self, root: &str, state: &str) -> Result<()> {
        self.set_chat_state_targets(root, state, &[], None)
    }
    fn set_chat_state_targets(
        &self,
        root: &str,
        state: &str,
        targets: &[(String, i64)],
        archived_at: Option<&str>,
    ) -> Result<()> {
        let layout = self
            .0
            .layout
            .as_ref()
            .context("Archive requires a directory store")?;
        let mut catalog = layout.catalog.lock().unwrap();
        let tx = catalog.transaction()?;
        tx.execute(
            "UPDATE sessions SET storage_state=?1 WHERE root_id=?2",
            params![state, root],
        )?;
        if let Some(time) = archived_at {
            tx.execute(
                "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
                params![format!("archived_at:{root}"), time],
            )?;
        }
        for (session, target) in targets {
            // The archive transition is already FULL. Its canonical sequence
            // receipt survives a NORMAL search projection rollback without
            // requiring known packed chats to be decoded on every startup.
            tx.execute(
                "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
                params![format!("search_target:{session}"), target.to_string()],
            )?;
        }
        tx.commit()?;
        layout.catalog_transactions.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    pub(super) fn restore_chat(&self, root: &str, path: &Path) -> Result<()> {
        self.flush_serving()?;
        let archive = cold::archive_path(path);
        let part = cold::staged_path(path, "restore-part");
        if part.exists() {
            std::fs::remove_file(&part)?;
        }
        if let Err(error) = cold::unpack(&archive, &part) {
            let _ = std::fs::remove_file(&part);
            return Err(error);
        }
        let conn = open_record(&part, true, true)?;
        let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        anyhow::ensure!(
            integrity == "ok",
            "Archived chat failed SQLite integrity check"
        );
        drop(conn);
        std::fs::rename(&part, path)?;
        cold::sync_parent(path)?;
        self.set_chat_state(root, "active")?;
        std::fs::remove_file(archive)?;
        cold::sync_parent(path)?;
        Ok(())
    }
    /// Read session metadata without restoring archived storage. Missing sessions
    /// and chat trees whose storage is archiving or archived return `None`.
    pub fn active_session_metadata(&self, session: &str) -> Result<Option<Session>> {
        let host = self.host().unwrap_or_else(|| self.clone());
        let data: Option<String> = if let Some(layout) = &host.0.layout {
            layout
                .catalog
                .lock()
                .unwrap()
                .query_row(
                    "SELECT s.data FROM sessions s JOIN sessions root ON root.id=s.root_id WHERE s.id=?1 AND s.storage_state='active' AND root.storage_state='active'",
                    [session],
                    |row| row.get(0),
                )
                .optional()?
        } else {
            host.read(|conn| {
                Ok(conn
                    .query_row("SELECT data FROM sessions WHERE id=?1", [session], |row| {
                        row.get(0)
                    })
                    .optional()?)
            })?
        };
        data.map(|data| Ok(serde_json::from_str(&data)?))
            .transpose()
    }

    /// Read a bounded page of attachment identifiers and metadata, without blobs.
    /// The prefix is literal; `after_id` is an exclusive keyset cursor.
    pub fn attachment_metadata_page(
        &self,
        session: &str,
        prefix: &str,
        after_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        if let Some(store) = self.scoped(session)? {
            return store.attachment_metadata_page(session, prefix, after_id, limit);
        }
        let limit = i64::try_from(limit).context("Attachment page limit exceeds SQLite range")?;
        self.read(|conn| {
            let mut statement = conn.prepare(
                "SELECT id,metadata FROM attachments WHERE session_id=?1 AND substr(id,1,length(?2))=?2 AND (?3 IS NULL OR id>?3) ORDER BY id LIMIT ?4",
            )?;
            statement
                .query_map(params![session, prefix, after_id, limit], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .map(|row| Ok(row?))
                .collect()
        })
    }

    /// Read the final attachment key and metadata under a literal prefix.
    /// Hosts can allocate ordered keys without loading the complete inventory.
    pub fn attachment_metadata_last(
        &self,
        session: &str,
        prefix: &str,
    ) -> Result<Option<(String, String)>> {
        if let Some(store) = self.scoped(session)? {
            return store.attachment_metadata_last(session, prefix);
        }
        self.read(|conn| {
            Ok(conn.query_row(
                "SELECT id,metadata FROM attachments WHERE session_id=?1 AND substr(id,1,length(?2))=?2 ORDER BY id DESC LIMIT 1",
                params![session, prefix],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?)
        })
    }

    /// Archive one complete chat tree after ensuring it has no pending work.
    /// Returns false when another reader or unresolved work keeps it active.
    pub fn archive_chat(&self, session: &str) -> Result<bool> {
        self.flush_serving()?;
        let layout = self
            .0
            .layout
            .as_ref()
            .context("Archive requires a directory store")?;
        let placement: Option<(String, String, String)> = layout
            .catalog
            .lock()
            .unwrap()
            .query_row(
                "SELECT root_id,file,storage_state FROM sessions WHERE id=?1",
                [session],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (root, file, state) = placement.context("Unknown session")?;
        if state == "archived" {
            return Ok(false);
        }
        let chat = self.open_chat_file(&root, &file, false)?;
        let sessions = chat.list_sessions()?;
        let ids: Vec<_> = sessions.iter().map(|session| session.id.clone()).collect();
        let runtime = self.runtime();
        let Ok(_active) = runtime.delete_guard(&ids) else {
            return Ok(false);
        };
        let pending=chat.read(|conn|Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM metadata WHERE substr(key,1,11)='quota_wait:') OR EXISTS(SELECT 1 FROM runs WHERE status IN ('running','waiting_for_quota')) OR EXISTS(SELECT 1 FROM inputs WHERE state IN ('pending','reminder_pending')) OR EXISTS(SELECT 1 FROM approvals WHERE state='pending') OR EXISTS(SELECT 1 FROM effects WHERE state='dispatched')",[],|row|row.get::<_,bool>(0))?))?;
        if pending {
            return Ok(false);
        }
        // The cache and this local handle are the only permitted references.
        if Arc::strong_count(&chat.0) != 2 {
            return Ok(false);
        }
        // Timestamp the archive transition rather than the last message age.
        // Existing metadata keeps this rebuildable without a schema migration.
        let archived_at = now();
        chat.write(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO metadata VALUES('archived_at',?1)",
                [&archived_at],
            )?;
            Ok(())
        })?;
        // Drain the metadata write's queued active projection before changing
        // placement, so a later flush cannot overwrite the archive transition.
        self.flush_serving()?;
        chat.read(|conn| {
            let busy: i64 =
                conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
            anyhow::ensure!(busy == 0, "Chat checkpoint is busy");
            Ok(())
        })?;
        let targets = chat.read(|conn| {
            ids.iter()
                .map(|session| {
                    Ok((
                        session.clone(),
                        conn.query_row(
                            "SELECT COALESCE(MAX(seq),0) FROM messages WHERE session_id=?1",
                            [session],
                            |row| row.get::<_, i64>(0),
                        )?,
                    ))
                })
                .collect::<Result<Vec<_>>>()
        })?;
        let path = chat.0.path.clone().context("Chat path missing")?;
        let mut chats = layout.chats.lock().unwrap();
        if Arc::strong_count(&chat.0) != 2 {
            return Ok(false);
        }
        chats.remove(&root);
        drop(chat);
        layout.forget_root(&root);
        // Hold cache placement while physical paths transition; no second opener.
        self.set_chat_state_targets(&root, "archiving", &targets, Some(&archived_at))?;
        cold::pack(&path)?;
        for suffix in ["-wal", "-shm"] {
            let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
            if sidecar.exists() {
                std::fs::remove_file(sidecar)?;
            }
        }
        std::fs::remove_file(&path)?;
        cold::sync_parent(&path)?;
        self.set_chat_state(&root, "archived")?;
        drop(chats);
        Ok(true)
    }
    /// Archived root IDs older than a strict RFC3339 cutoff. Older archives with
    /// no recorded transition time remain ineligible for automatic deletion.
    /// This reads metadata without restoring archived content.
    pub fn archived_roots_before(&self, cutoff: &str) -> Result<Vec<String>> {
        self.flush_serving()?;
        let layout = self
            .0
            .layout
            .as_ref()
            .context("Archive retention requires a directory store")?;
        let catalog = layout.catalog.lock().unwrap();
        let mut statement = catalog.prepare("SELECT s.id FROM sessions s JOIN metadata m ON m.key='archived_at:'||s.id WHERE s.id=s.root_id AND s.storage_state='archived' AND julianday(m.value)<julianday(?1) AND NOT EXISTS(SELECT 1 FROM sessions child WHERE child.root_id=s.id AND child.storage_state!='archived') ORDER BY s.id")?;
        Ok(statement
            .query_map([cutoff], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }
    /// Archive roots whose entire tree has been idle beyond the configured age.
    pub fn archive_idle_chats(&self) -> Result<usize> {
        let layout = self
            .0
            .layout
            .as_ref()
            .context("Archive requires a directory store")?;
        let roots: Vec<(String, String, Option<String>)> = {
            let catalog = layout.catalog.lock().unwrap();
            let mut statement=catalog.prepare("SELECT s.root_id,MAX(s.last_activity),m.value FROM sessions s LEFT JOIN metadata m ON m.key='root_storage:'||s.root_id GROUP BY s.root_id HAVING MIN(s.storage_state)='active' AND MAX(s.storage_state)='active'")?;
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut count = 0;
        let instant = chrono::Utc::now();
        for (root, activity, value) in roots {
            let policy = value
                .map(|value| serde_json::from_str::<Option<RootStoragePolicy>>(&value))
                .transpose()?
                .flatten();
            let days = policy.map_or(layout.options.archive_after_days, |policy| {
                policy.store.archive_after_days
            });
            if days == 0 {
                continue;
            }
            let cutoff = instant
                .checked_sub_signed(chrono::Duration::days(i64::from(days)))
                .context("Archive retention is outside the supported date range")?
                .to_rfc3339();
            if activity >= cutoff {
                continue;
            }
            if self.archive_chat(&root)? {
                count += 1;
            }
        }
        Ok(count)
    }
    pub(super) fn recover_archives(&self) -> Result<()> {
        let layout = self.0.layout.as_ref().unwrap();
        let rows: Vec<(String, String, String)> = {
            let catalog = layout.catalog.lock().unwrap();
            let mut statement = catalog
                .prepare("SELECT root_id,file,storage_state FROM sessions GROUP BY root_id")?;
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        for (root, file, state) in rows {
            anyhow::ensure!(
                ["active", "archiving", "archived"].contains(&state.as_str()),
                "Unsupported chat storage state: {state}"
            );
            let relative = Path::new(&file);
            anyhow::ensure!(
                relative
                    .components()
                    .all(|component| matches!(component, std::path::Component::Normal(_))),
                "Invalid catalog chat path"
            );
            let path = layout.data_dir.join(relative);
            let archive = cold::archive_path(&path);
            if state == "archiving" && path.is_file() {
                let valid = archive.is_file() && cold::verify_archive(&path, &archive).is_ok();
                if valid {
                    layout.chats.lock().unwrap().remove(&root);
                    layout.forget_root(&root);
                    std::fs::remove_file(&path)?;
                    for suffix in ["-wal", "-shm"] {
                        let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
                        if sidecar.exists() {
                            std::fs::remove_file(sidecar)?;
                        }
                    }
                    cold::sync_parent(&path)?;
                    self.set_chat_state(&root, "archived")?;
                } else {
                    if archive.is_file() {
                        std::fs::remove_file(&archive)?;
                    }
                    self.set_chat_state(&root, "active")?;
                }
            } else if state == "archiving" && archive.is_file() {
                let conn = open_record(&path, true, true)?;
                let integrity: String =
                    conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
                anyhow::ensure!(
                    integrity == "ok",
                    "Interrupted archive failed integrity check"
                );
                self.set_chat_state(&root, "archived")?;
            } else if state == "archived" && path.is_file() {
                // Restore installed a verified original before updating catalog.
                if archive.is_file() {
                    cold::verify_archive(&path, &archive)?;
                    std::fs::remove_file(&archive)?;
                }
                self.set_chat_state(&root, "active")?;
            }
            anyhow::ensure!(
                state == "active" || path.is_file() || archive.is_file(),
                "Interrupted archive has neither original nor compressed record"
            );
            let part = cold::staged_path(&archive, "part");
            if part.exists() && (path.is_file() || archive.is_file()) {
                std::fs::remove_file(part)?;
            }
            let restore = cold::staged_path(&path, "restore-part");
            if restore.exists() && archive.is_file() {
                std::fs::remove_file(restore)?;
            }
        }
        Ok(())
    }
}
