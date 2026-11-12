//! Layout checkpoints, deletion and storage statistics.
use super::*;

/// Whether any checkpoint record still names a preimage. It must stay bound to
/// one hash and use the `checkpoint_records_preimage` index.
pub(super) const CHECKPOINT_PREIMAGE_NAMED_SQL: &str =
    "SELECT EXISTS(SELECT 1 FROM checkpoint_records WHERE preimage_hash=?1)";

impl Store {
    pub(in crate::store) fn layout_checkpoint(&self) -> Result<()> {
        self.checkpoint_layout(true)
    }

    pub(in crate::store) fn layout_runtime_idle(&self) -> Result<()> {
        self.checkpoint_layout(false)
    }

    fn checkpoint_layout(&self, force: bool) -> Result<()> {
        self.housekeeping()?;
        self.flush_serving()?;
        let layout = self.0.layout.as_ref().unwrap();
        let chats: Vec<_> = layout.chats.lock().unwrap().values().cloned().collect();
        for chat in chats {
            if force || wal_needs_checkpoint(chat.0.path.as_deref()) {
                chat.idle_checkpoint()?;
            }
        }
        if force || wal_needs_checkpoint(self.0.path.as_deref()) {
            self.read(|conn| {
                conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
                Ok(())
            })?;
        }
        if force || wal_needs_checkpoint(Some(&layout.data_dir.join("catalog.db"))) {
            layout
                .catalog
                .lock()
                .unwrap()
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        }
        self.read(|conn| {
            let free: i64 = conn.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
            if free > 32 {
                conn.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")?;
            }
            Ok(())
        })?;
        Ok(())
    }
    pub(in crate::store) fn layout_delete(
        &self,
        session_id: &str,
        preserve_effects: bool,
    ) -> Result<()> {
        self.flush_serving()?;
        let runtime = self.runtime();
        let last_event = runtime.log.last();
        let layout = self.0.layout.as_ref().unwrap();
        let chat = self.scoped(session_id)?.unwrap();
        let sessions = chat.list_sessions()?;
        let mut removed = vec![session_id.to_owned()];
        loop {
            let before = removed.len();
            for session in &sessions {
                if session
                    .parent_session_id
                    .as_ref()
                    .is_some_and(|parent| removed.contains(parent))
                    && !removed.contains(&session.id)
                {
                    removed.push(session.id.clone());
                }
            }
            if before == removed.len() {
                break;
            }
        }
        let mut admission = runtime.delete_guard(&removed)?;
        for session in &removed {
            let running = chat.read(|conn| {
                Ok(conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM runs WHERE session_id=?1 AND status IN ('running','waiting_for_quota'))",
                    [session],
                    |row| row.get::<_, bool>(0),
                )?)
            })?;
            anyhow::ensure!(!running, "Cannot delete a session with a running run");
            if preserve_effects {
                let unresolved = chat.read(|conn| Ok(conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM effects WHERE session_id=?1 AND state!='completed')",
                    [session], |row| row.get::<_, bool>(0),
                )?))?;
                anyhow::ensure!(
                    !unresolved,
                    "Cannot delete a session with unresolved effects"
                );
            }
        }
        // Deleted children stay in an open chat; their placements must not.
        layout.forget_sessions(&removed);
        let path = chat.0.path.clone().unwrap();
        let root = path.file_stem().unwrap().to_string_lossy().into_owned();
        let _deleting = (session_id == root).then(|| release::Deleting::evict(layout, &root));
        if session_id == root {
            layout.forget_root(&root);
            // Close every handle to the chat before deleting its SQLite files.
            release::remove_chat_files(chat, &path)?;
        } else {
            chat.write(|tx| {
                if preserve_effects {
                    for session in &removed {
                        let unresolved: bool = tx.query_row(
                            "SELECT EXISTS(SELECT 1 FROM effects WHERE session_id=?1 AND state!='completed')",
                            [session], |row| row.get(0),
                        )?;
                        anyhow::ensure!(!unresolved, "Cannot delete a session with unresolved effects");
                    }
                }
                let imported: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='imported_derived')",
                    [],
                    |row| row.get(0),
                )?;
                for session in &removed {
                    Self::delete_execution_mode_tx(tx, session)?;
                    Self::clear_quota_wait_tx(tx, session, None)?;
                    if imported {
                        tx.execute(
                            "DELETE FROM imported_derived WHERE session_id=?1",
                            [session],
                        )?;
                    }
                    tx.execute(
                        "DELETE FROM metadata WHERE key=?1",
                        [format!("import:{session}")],
                    )?;
                    tx.execute(
                        "DELETE FROM metadata WHERE key=?1",
                        [format!("cold_compressed_through:{session}")],
                    )?;
                    tx.execute(
                        "DELETE FROM metadata WHERE key=?1",
                        [format!("cold_compression_pass:{session}")],
                    )?;
                }
                for session in &removed {
                    chat.delete_session_checkpoints(tx, session)?;
                    tx.execute("UPDATE request_usage SET message_seq=NULL WHERE session_id=?1 AND message_seq IS NOT NULL", [session])?;
                }
                for table in [
                    "reminder_metadata",
                    "approvals",
                    "approval_rules",
                        "host_state",
                    "attachments",
                    "context_checkpoint_versions",
                    "context_decisions",
                    "context_events",
                    "context_episodes",
                    "context_subject_chapters",
                    "context_chapters",
                    "context_checkpoints",
                    "outputs",
                    "effects",
                    "inputs",
                    "runs",
                    "messages",
                    "sessions",
                ] {
                    let column = if table == "sessions" {
                        "id"
                    } else {
                        "session_id"
                    };
                    for session in &removed {
                        tx.execute(&format!("DELETE FROM {table} WHERE {column}=?1"), [session])?;
                    }
                }
                chat.collect_orphan_blobs(tx)?;
                Ok(())
            })?;
            chat.read(|conn| {
                // incremental_vacuum emits rows while reclaiming pages. Exhaust
                // the statement instead of stopping after its first sqlite step.
                let mut statement = conn.prepare("PRAGMA incremental_vacuum")?;
                let mut rows = statement.query([])?;
                while rows.next()?.is_some() {}
                drop(rows);
                drop(statement);
                conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
                Ok(())
            })?;
        }
        self.write(|tx| {
            Self::remove_search_sessions(tx, &removed)?;
            for table in [
                "events",
                "context_views",
                "runs",
                "inputs",
                "approvals",
                "telemetry",
                "sessions",
            ] {
                let column = if table == "sessions" {
                    "id"
                } else {
                    "session_id"
                };
                for session in &removed {
                    tx.execute(&format!("DELETE FROM {table} WHERE {column}=?1"), [session])?;
                }
            }
            crate::event_log::remember_last(tx, last_event)?;
            Ok(())
        })?;
        self.catalog_rebuild_mode(false)?;
        for session in &removed {
            admission.remove(session);
            runtime.forget_history(session);
        }
        self.cleanup_deleted_sessions(&removed)?;
        Ok(())
    }
    /// The text of every field in this chat that can name a blob hash, with
    /// message bodies decoded. Hash references are substrings of this text.
    /// Number of reference-text scans, for tests that assert pruning never scans.
    #[cfg(test)]
    pub(crate) fn reference_scans() -> usize {
        REFERENCE_SCANS.with(std::cell::Cell::get)
    }
    pub(crate) fn reference_text(&self, conn: &Connection) -> Result<String> {
        #[cfg(test)]
        REFERENCE_SCANS.with(|count| count.set(count.get() + 1));
        let mut references = String::new();
        for (table, column) in [
            ("messages", "data"),
            ("attachments", "blob_hash"),
            ("inputs", "content_blob"),
            ("context_checkpoints", "data"),
            ("context_checkpoint_versions", "summary_hash"),
            ("approvals", "arguments_ref"),
            ("effects", "detail"),
            ("runs", "data"),
        ] {
            let mut stmt = conn.prepare(&format!(
                "SELECT {column} FROM {table} WHERE {column} IS NOT NULL"
            ))?;
            for value in stmt.query_map([], |row| row.get::<_, String>(0))? {
                let value = value?;
                if table == "messages" {
                    references.push_str(&Self::decode_message_body(conn, &value)?.to_string());
                } else {
                    references.push_str(&value);
                }
                // Separate values, so adjacent hashes never run together.
                references.push('\n');
            }
        }
        if let Some(host) = self.host() {
            let sessions = {
                let mut stmt = conn.prepare("SELECT id FROM sessions")?;
                stmt.query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            host.read(|projection| {
                for (table, column) in [
                    ("events", "data"),
                    ("approvals", "arguments_ref"),
                    ("runs", "data"),
                ] {
                    let mut stmt = projection.prepare(&format!(
                        "SELECT {column} FROM {table} WHERE session_id=?1 AND {column} IS NOT NULL"
                    ))?;
                    for session in &sessions {
                        for row in stmt.query_map([session], |row| row.get::<_, String>(0))? {
                            references.push_str(&row?);
                            references.push('\n');
                        }
                    }
                }
                Ok(())
            })?;
        }
        Ok(references)
    }
    pub(crate) fn collect_orphan_blobs(&self, conn: &Connection) -> Result<()> {
        // Conservative reachability: content references occur in JSON/text fields.
        // False positives retain a blob; no false negative may remove live content.
        let mut references = self.reference_text(conn)?;
        let mut stmt = conn.prepare("SELECT hash FROM blobs")?;
        let hashes = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // Content-part blobs can themselves reference attachment blobs. Traverse
        // those edges before reclaiming any hash; arbitrary binary data is safe.
        let mut live = std::collections::HashSet::new();
        loop {
            let mut changed = false;
            for hash in &hashes {
                if references.contains(hash) && live.insert(hash.clone()) {
                    references.push_str(&String::from_utf8_lossy(&Self::blob_get(conn, hash)?));
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for hash in hashes {
            if !live.contains(&hash) {
                conn.execute("DELETE FROM blobs WHERE hash=?1", [hash])?;
            }
        }
        Ok(())
    }
    /// Delete each candidate checkpoint preimage that no checkpoint record names.
    /// The lookup uses the `checkpoint_records_preimage` index and is bounded to
    /// one hash per candidate. Content rows and `blobs` are never read or changed.
    pub(crate) fn drop_unreferenced_checkpoint_blobs(
        &self,
        conn: &Connection,
        candidates: std::collections::BTreeSet<String>,
    ) -> Result<()> {
        for hash in &candidates {
            let named: bool =
                conn.query_row(CHECKPOINT_PREIMAGE_NAMED_SQL, [hash], |row| row.get(0))?;
            if !named {
                conn.execute("DELETE FROM checkpoint_blobs WHERE hash=?1", [hash])?;
            }
        }
        Ok(())
    }

    /// Remove a session's checkpoint records, then the preimages they alone named.
    pub(crate) fn delete_session_checkpoints(
        &self,
        conn: &Connection,
        session_id: &str,
    ) -> Result<()> {
        let candidates: std::collections::BTreeSet<String> = {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT preimage_hash FROM checkpoint_records WHERE session_id=?1 AND preimage_hash IS NOT NULL",
            )?;
            stmt.query_map([session_id], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        conn.execute(
            "DELETE FROM checkpoint_records WHERE session_id=?1",
            [session_id],
        )?;
        self.drop_unreferenced_checkpoint_blobs(conn, candidates)
    }
    pub(in crate::store) fn layout_stats(&self) -> Result<StorageStats> {
        let layout = self.0.layout.as_ref().unwrap();
        let mut stats = StorageStats {
            page_bytes: 0,
            database_bytes: 0,
            wal_bytes: 0,
            transaction_count: self.revision(),
            message_count: 0,
            pending_input_count: 0,
            chat_bytes: 0,
            serving_bytes: 0,
            catalog_bytes: 0,
            chat_transaction_count: self
                .revision()
                .saturating_sub(layout.serving_transactions.load(Ordering::Relaxed)),
            catalog_transaction_count: layout.catalog_transactions.load(Ordering::Relaxed),
            serving_transaction_count: layout.serving_transactions.load(Ordering::Relaxed),
        };
        for entry in std::fs::read_dir(layout.data_dir.join("chats"))? {
            let path = entry?.path();
            if path.to_string_lossy().ends_with(".db.zst") {
                let unpacked = path.with_extension("");
                if unpacked.exists() {
                    continue;
                }
                let bytes = std::fs::metadata(&path)?.len();
                stats.database_bytes += bytes;
                stats.chat_bytes += bytes;
                let record = open_record(&unpacked, true, true)?;
                let pages: i64 = record.query_row("PRAGMA page_count", [], |row| row.get(0))?;
                let page_size: i64 = record.query_row("PRAGMA page_size", [], |row| row.get(0))?;
                stats.page_bytes += (pages * page_size) as u64;
                stats.message_count +=
                    record.query_row("SELECT COUNT(*) FROM messages", [], |row| {
                        row.get::<_, i64>(0)
                    })? as u64;
                stats.pending_input_count += record.query_row(
                    "SELECT COUNT(*) FROM inputs WHERE state='pending'",
                    [],
                    |row| row.get::<_, i64>(0),
                )? as u64;
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "db") {
                continue;
            }
            let root = path.file_stem().unwrap().to_string_lossy();
            let chat = self.open_chat(&root)?;
            let part = chat.storage_stats()?;
            stats.page_bytes += part.page_bytes;
            stats.database_bytes += part.database_bytes;
            stats.chat_bytes += part.database_bytes;
            stats.wal_bytes += part.wal_bytes;
            stats.message_count += part.message_count;
            stats.pending_input_count += part.pending_input_count;
        }
        for (name, target) in [
            ("serving.db", &mut stats.serving_bytes),
            ("catalog.db", &mut stats.catalog_bytes),
        ] {
            let path = layout.data_dir.join(name);
            *target = std::fs::metadata(&path)?.len();
            stats.database_bytes += *target;
            stats.wal_bytes += std::fs::metadata(format!("{}-wal", path.display()))
                .map(|metadata| metadata.len())
                .unwrap_or(0);
        }
        Ok(stats)
    }
}
