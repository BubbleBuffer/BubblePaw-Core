//! Offline exports include structured reminder identities alongside messages.
use super::*;
use base64::Engine;
use std::io::Write;

impl Store {
    pub fn export_jsonl(&self, session: &str, mut writer: impl Write) -> Result<usize> {
        let metadata = self.session(session)?;
        let sessions = self.list_sessions()?;
        let child_session_ids: Vec<_> = sessions
            .iter()
            .filter(|child| child.parent_session_id.as_deref() == Some(session))
            .map(|child| &child.id)
            .collect();
        let root_id = self
            .chat_path(session)?
            .and_then(|path| path.file_stem().map(|id| id.to_string_lossy().into_owned()))
            .unwrap_or_else(|| session.into());
        let chat = self.scoped(session)?.unwrap_or_else(|| self.clone());
        let attachments = chat.read(|conn| Self::export_attachments(conn, session))?;
        let checkpoints = chat.read(|conn| Self::export_checkpoints(conn, session))?;
        let provenance = self.import_metadata(session)?;
        let mut index = 0;
        let count = chat.read(|conn| {
            let mut statement = conn.prepare("SELECT data FROM messages WHERE session_id=?1 ORDER BY seq")?;
            let mut rows = statement.query([session])?;
            while let Some(row) = rows.next()? {
            let message = Self::message_from_data(conn, &row.get::<_, String>(0)?)?;
            let reminder = Self::reminder_metadata_tx(conn, session, &message.id)?;
            let mut record = serde_json::json!({"session_id":session,"parent_session_id":metadata.parent_session_id,"root_id":root_id,"child_session_ids":child_session_ids,"message":message});
            if let Some(reminder) = reminder { record["reminder_metadata"] = serde_json::to_value(reminder)?; }
            if index == 0 && !attachments.is_empty() {
                record["attachments"] = attachments.clone().into();
            }
            if index == 0 && !checkpoints.is_empty() {
                record["checkpoints"] = checkpoints.clone().into();
            }
            if index == 0 && let Some(provenance)=&provenance { record["import_provenance"]=provenance.clone(); }
            serde_json::to_writer(&mut writer, &record)?;
            writer.write_all(b"\n")?;
            index += 1;
            }
            Ok(index)
        })?;
        if count == 0
            && let Some(provenance) = &provenance
        {
            serde_json::to_writer(
                &mut writer,
                &serde_json::json!({"kind":"import_provenance","session_id":session,"root_id":root_id,"import_provenance":provenance}),
            )?;
            writer.write_all(b"\n")?;
        }
        self.for_each_import_derived(session,|derived| {
            serde_json::to_writer(&mut writer,&serde_json::json!({"kind":"import_derived","session_id":session,"root_id":root_id,"derived":derived}))?;
            writer.write_all(b"\n")?;
            Ok(())
        })?;
        Ok(count)
    }
    /// Opens only the record file read-only. No recovery, retention or migrations.
    pub fn export_data_dir_jsonl(
        data_dir: impl AsRef<Path>,
        session: &str,
        mut writer: impl Write,
    ) -> Result<usize> {
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(data_dir.as_ref().join("store.lock"))
            .ok()
            .and_then(|file| FileLock::acquire_shared(file).ok());
        let immutable_allowed = lock.is_some();
        let mut found = None;
        let mut root_id = String::new();
        let mut child_session_ids = Vec::new();
        let catalog_path = data_dir.as_ref().join("catalog.db");
        if catalog_path.is_file() {
            let catalog = open_read_only(&catalog_path, immutable_allowed)?;
            let application: i64 =
                catalog.query_row("PRAGMA application_id", [], |row| row.get(0))?;
            let version: i64 = catalog.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            anyhow::ensure!(
                application == 0x42504231 && (1..=2).contains(&version),
                "Unsupported catalog schema"
            );
            let catalog = Mutex::new(catalog);
            if let Some((conn, root)) =
                open_session_record(&catalog, data_dir.as_ref(), session, |_, path| {
                    open_record(path, true, immutable_allowed)
                })?
            {
                let exists: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
                    [session],
                    |row| row.get(0),
                )?;
                if exists {
                    let catalog = catalog.lock().unwrap();
                    let mut children = catalog
                        .prepare("SELECT id FROM sessions WHERE parent_id=?1 ORDER BY id")?;
                    child_session_ids = children
                        .query_map([session], |row| row.get::<_, String>(0))?
                        .collect::<rusqlite::Result<_>>()?;
                    root_id = root;
                    found = Some(conn);
                }
            }
        }
        if found.is_none() {
            for entry in std::fs::read_dir(data_dir.as_ref().join("chats"))? {
                let mut path = entry?.path();
                if path.to_string_lossy().ends_with(".db.zst") {
                    path = path.with_extension("");
                    if path.is_file() {
                        continue;
                    }
                } else if path.extension().is_none_or(|extension| extension != "db") {
                    continue;
                }
                let conn = open_record(&path, true, immutable_allowed)?;
                let exists: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
                    [session],
                    |row| row.get(0),
                )?;
                let mut children = conn.prepare("SELECT id FROM sessions WHERE json_extract(data,'$.parent_session_id')=?1 ORDER BY id")?;
                child_session_ids.extend(
                    children
                        .query_map([session], |row| row.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?,
                );
                drop(children);
                if exists {
                    root_id = path.file_stem().unwrap().to_string_lossy().into_owned();
                    found = Some(conn);
                }
            }
        }
        let conn = found.with_context(|| format!("Unknown session: {session}"))?;
        let metadata = Self::session_from_connection(&conn, session)?;
        child_session_ids.sort();
        child_session_ids.dedup();
        let mut statement =
            conn.prepare("SELECT data FROM messages WHERE session_id=?1 ORDER BY seq")?;
        let mut count = 0;
        let attachments = Self::export_attachments(&conn, session)?;
        let checkpoints = Self::export_checkpoints(&conn, session)?;
        let provenance: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [format!("import:{session}")],
                |row| row.get(0),
            )
            .optional()?;
        let provenance = provenance
            .map(|data| serde_json::from_str::<serde_json::Value>(&data))
            .transpose()?;
        for row in statement.query_map([session], |row| row.get::<_, String>(0))? {
            let message = Self::message_from_data(&conn, &row?)?;
            let mut record = serde_json::json!({"session_id":session,"parent_session_id":metadata.parent_session_id,"root_id":root_id,"child_session_ids":child_session_ids,"message":message});
            let has_reminders: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='reminder_metadata')",
                [],
                |row| row.get(0),
            )?;
            if has_reminders
                && let Some(reminder) = Self::reminder_metadata_tx(&conn, session, &message.id)?
            {
                record["reminder_metadata"] = serde_json::to_value(reminder)?;
            }
            if count == 0 && !attachments.is_empty() {
                record["attachments"] = attachments.clone().into();
            }
            if count == 0 && !checkpoints.is_empty() {
                record["checkpoints"] = checkpoints.clone().into();
            }
            if count == 0
                && let Some(provenance) = &provenance
            {
                record["import_provenance"] = provenance.clone();
            }
            serde_json::to_writer(&mut writer, &record)?;
            writer.write_all(b"\n")?;
            count += 1;
        }
        if count == 0
            && let Some(provenance) = &provenance
        {
            serde_json::to_writer(
                &mut writer,
                &serde_json::json!({"kind":"import_provenance","session_id":session,"root_id":root_id,"import_provenance":provenance}),
            )?;
            writer.write_all(b"\n")?;
        }
        let imported: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='imported_derived')",
            [],
            |row| row.get(0),
        )?;
        if imported {
            let mut statement = conn.prepare(
                "SELECT data FROM imported_derived WHERE session_id=?1 ORDER BY source_line,id",
            )?;
            let mut rows = statement.query([session])?;
            while let Some(row) = rows.next()? {
                let derived: serde_json::Value = serde_json::from_str(&row.get::<_, String>(0)?)?;
                serde_json::to_writer(
                    &mut writer,
                    &serde_json::json!({"kind":"import_derived","session_id":session,"root_id":root_id,"derived":derived}),
                )?;
                writer.write_all(b"\n")?;
            }
        }
        Ok(count)
    }
    /// A session's checkpoint records, each with its preimage in base64, as the
    /// attachments section carries its blobs.
    fn export_checkpoints(conn: &Connection, session: &str) -> Result<Vec<serde_json::Value>> {
        // A chat that was never opened by this build has no checkpoint tables. Its
        // legacy checkpoints are exported as attachments, so this section is empty.
        let has_table: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='checkpoint_records')",
            [],
            |row| row.get(0),
        )?;
        if !has_table {
            return Ok(Vec::new());
        }
        let mut statement = conn.prepare(
            "SELECT id,boundary_seq,metadata,preimage_hash FROM checkpoint_records WHERE session_id=?1 ORDER BY id",
        )?;
        let rows = statement
            .query_map([session], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(id, boundary_seq, metadata, hash)| {
                let metadata = serde_json::from_str::<serde_json::Value>(&metadata)
                    .unwrap_or(serde_json::Value::String(metadata));
                // An unrestorable record has no preimage, so there is nothing to hydrate.
                let Some(hash) = hash else {
                    return Ok(serde_json::json!({"id":id,"boundary_seq":boundary_seq,"metadata":metadata,"preimage_hash":null}));
                };
                let preimage = Self::checkpoint_get(conn, &hash)?;
                Ok(serde_json::json!({"id":id,"boundary_seq":boundary_seq,"metadata":metadata,"preimage_hash":hash,"encoding":"base64","data":base64::engine::general_purpose::STANDARD.encode(preimage)}))
            })
            .collect()
    }

    fn export_attachments(conn: &Connection, session: &str) -> Result<Vec<serde_json::Value>> {
        let mut statement = conn.prepare(
            "SELECT id,metadata,blob_hash FROM attachments WHERE session_id=?1 ORDER BY id",
        )?;
        statement.query_map([session], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)))?.map(|row| {
            let (id, metadata, hash) = row?;
            let metadata = serde_json::from_str::<serde_json::Value>(&metadata).unwrap_or(serde_json::Value::String(metadata));
            Ok(serde_json::json!({"id":id,"metadata":metadata,"blob_hash":hash,"encoding":"base64","data":base64::engine::general_purpose::STANDARD.encode(Self::blob_get(conn, &hash)?)}))
        }).collect()
    }
}
