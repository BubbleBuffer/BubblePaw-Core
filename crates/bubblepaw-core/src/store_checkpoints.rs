//! File checkpoint records and the legacy checkpoint sweep.
use super::*;

impl Store {
    /// Key of a session's checkpoint high-water mark in `metadata`.
    fn checkpoint_high_water_key(session_id: &str) -> String {
        format!("checkpoint_high_water:{session_id}")
    }

    /// Highest checkpoint patch index this session has written (0 if none). It
    /// survives pruning, so a later checkpoint never reuses an index.
    pub fn checkpoint_high_water(&self, session_id: &str) -> Result<u64> {
        Ok(self
            .chat_metadata(session_id, &Self::checkpoint_high_water_key(session_id))?
            .map(|value| value.parse::<u64>())
            .transpose()?
            .unwrap_or(0))
    }

    /// Write one checkpoint in one write transaction. Its preimage goes to
    /// `checkpoint_blobs` and its record to `checkpoint_records`. Each `(session,
    /// keep_from)` in `prunes` first deletes that session's records whose boundary
    /// sequence is below `keep_from`, judged on the rows as they are inside this
    /// transaction; callers pass a bound, never record IDs. Preimages that no record
    /// still names are then dropped. The record's patch index must not be below the
    /// session's high-water mark, which then rises to it. Returns the preimage hash.
    /// Sessions in `prunes` must belong to this chat.
    pub fn write_checkpoint(
        &self,
        session_id: &str,
        record: CheckpointRecord<'_>,
        bytes: &[u8],
        prunes: &[(String, i64)],
    ) -> Result<String> {
        if let Some(store) = self.scoped(session_id)? {
            return store.write_checkpoint(session_id, record, bytes, prunes);
        }
        self.write(|tx| {
            let key = Self::checkpoint_high_water_key(session_id);
            let high_water = tx
                .query_row("SELECT value FROM metadata WHERE key=?1", [&key], |row| {
                    row.get::<_, String>(0)
                })
                .optional()?
                .map(|value| value.parse::<u64>())
                .transpose()?
                .unwrap_or(0);
            anyhow::ensure!(
                record.patch_index >= high_water,
                "Checkpoint patch {} is below the session high-water mark {high_water}",
                record.patch_index
            );
            let mut candidates = std::collections::BTreeSet::new();
            for (pruned_session, keep_from) in prunes {
                let stale: Vec<(String, Option<String>)> = {
                    let mut stmt = tx.prepare(
                        "SELECT id,preimage_hash FROM checkpoint_records WHERE session_id=?1 AND boundary_seq<?2",
                    )?;
                    stmt.query_map(params![pruned_session, keep_from], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })?
                    .collect::<rusqlite::Result<_>>()?
                };
                for (id, preimage) in stale {
                    tx.execute(
                        "DELETE FROM checkpoint_records WHERE session_id=?1 AND id=?2",
                        params![pruned_session, id],
                    )?;
                    // An unrestorable record has no preimage to release.
                    candidates.extend(preimage);
                }
            }
            let hash = Self::checkpoint_put(tx, bytes)?;
            tx.execute(
                "INSERT INTO checkpoint_records(session_id,id,boundary_seq,metadata,preimage_hash) VALUES(?1,?2,?3,?4,?5)",
                params![session_id, record.id, record.boundary_seq, record.metadata, hash],
            )?;
            tx.execute(
                "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
                params![key, record.patch_index.to_string()],
            )?;
            self.drop_unreferenced_checkpoint_blobs(tx, candidates)?;
            Ok(hash)
        })
    }

    /// One-time sweep, run by the host when a chat opens. `keep` gives each session
    /// of the chat its keep bound, as `write_checkpoint` reads it; a session not
    /// listed keeps all its legacy rows. Each legacy row is an attachment under the
    /// checkpoint prefix, as an earlier build wrote it. Row by row, in one transaction:
    /// - A row at or above its bound is copied into `checkpoint_records`. Its preimage
    ///   is copied from `blobs` into `checkpoint_blobs`. A created file has no
    ///   preimage, so a missing blob is filled with empty bytes.
    /// - A row at or above its bound whose preimage is needed but missing is copied
    ///   as an `unrestorable` record, marked in its metadata. Rewind refuses any turn
    ///   that contains it.
    /// - A row below its bound is deleted, unless it needs a missing preimage. Then it
    ///   is left in `attachments` and returned.
    /// - A row whose metadata is not a JSON object, or has no boundary sequence, is
    ///   left in `attachments` and returned. Its patch index still raises the
    ///   high-water mark when readable. Its turn is unknown, so its session gets one
    ///   unrestorable record from `unreadable`, when that session has such a row.
    ///
    /// `unreadable` gives (session, boundary sequence, metadata) for the record of each
    /// session that can have one. Each session's high-water mark rises to its highest
    /// legacy patch index, and `marker` is set. Only SQLite errors fail the sweep,
    /// which rolls back. `blobs` is never changed.
    pub fn sweep_legacy_checkpoints(
        &self,
        session_id: &str,
        marker: (&str, &str),
        keep: &[(String, i64)],
        unreadable: &[(String, i64, String)],
    ) -> Result<Vec<LegacyCheckpointNote>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.sweep_legacy_checkpoints(session_id, marker, keep, unreadable);
        }
        self.write(|tx| {
            let keep: std::collections::BTreeMap<&str, i64> = keep
                .iter()
                .map(|(session, from)| (session.as_str(), *from))
                .collect();
            let legacy: Vec<(String, String, String, String)> = {
                let mut stmt = tx.prepare(
                    "SELECT session_id,id,metadata,blob_hash FROM attachments WHERE substr(id,1,length(?1))=?1 ORDER BY session_id,id",
                )?;
                stmt.query_map([CHECKPOINT_PREFIX], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<rusqlite::Result<_>>()?
            };
            let mut high_water: std::collections::BTreeMap<String, u64> = Default::default();
            let mut handled: Vec<(&str, &str)> = Vec::new();
            // Sessions with a row whose turn cannot be read. Each gets one unrestorable
            // record below, so rewinding its latest turn refuses.
            let mut unreadable_sessions: std::collections::BTreeSet<String> = Default::default();
            let mut notes = Vec::new();
            for (session, id, metadata, hash) in &legacy {
                let note = |left: bool, reason: &str| LegacyCheckpointNote {
                    session_id: session.clone(),
                    id: id.clone(),
                    reason: reason.to_owned(),
                    left,
                };
                let value = serde_json::from_str::<serde_json::Value>(metadata)
                    .ok()
                    .filter(serde_json::Value::is_object);
                if let Some(patch) = value
                    .as_ref()
                    .and_then(|value| value.get("patch_index"))
                    .and_then(serde_json::Value::as_u64)
                {
                    let highest = high_water.entry(session.clone()).or_default();
                    *highest = (*highest).max(patch);
                }
                let Some(value) = value else {
                    unreadable_sessions.insert(session.clone());
                    notes.push(note(true, "metadata is not a JSON object"));
                    continue;
                };
                let Some(boundary) = value.get("boundary_seq").and_then(serde_json::Value::as_i64)
                else {
                    unreadable_sessions.insert(session.clone());
                    notes.push(note(true, "metadata has no boundary sequence"));
                    continue;
                };
                let from = keep.get(session.as_str()).copied().unwrap_or(i64::MIN);
                let in_range = boundary >= from;
                // A created file has no preimage: restore deletes it and never reads the
                // blob. Its legacy blob is empty, so a missing one can be recreated.
                let created = matches!(
                    value.get("pre_hash"),
                    None | Some(serde_json::Value::Null)
                );
                let blob_present: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM blobs WHERE hash=?1)",
                    [hash],
                    |row| row.get(0),
                )?;
                if !created && !blob_present {
                    if !in_range {
                        notes.push(note(true, "preimage blob is missing outside the kept range"));
                        continue;
                    }
                    // An unrestorable record references no preimage blob.
                    let mut marked = value.clone();
                    marked["unrestorable"] = serde_json::json!("preimage blob is missing");
                    tx.execute(
                        "INSERT INTO checkpoint_records(session_id,id,boundary_seq,metadata,preimage_hash) VALUES(?1,?2,?3,?4,NULL)",
                        params![session, id, boundary, marked.to_string()],
                    )?;
                    notes.push(note(
                        false,
                        "preimage blob is missing; migrated as unrestorable",
                    ));
                } else if in_range {
                    // The preimage is written through the checkpoint write path, so it is
                    // encoded exactly as a new checkpoint's preimage is. A created file's
                    // missing blob becomes the empty preimage, which later checkpoints of
                    // an existing empty file reuse.
                    let preimage = if blob_present {
                        tx.execute(
                            "INSERT OR IGNORE INTO checkpoint_blobs(hash,raw_bytes,data) SELECT hash,raw_bytes,data FROM blobs WHERE hash=?1",
                            [hash],
                        )?;
                        hash.clone()
                    } else {
                        Self::checkpoint_put(tx, &[])?
                    };
                    tx.execute(
                        "INSERT INTO checkpoint_records(session_id,id,boundary_seq,metadata,preimage_hash) VALUES(?1,?2,?3,?4,?5)",
                        params![session, id, boundary, metadata, preimage],
                    )?;
                }
                handled.push((session.as_str(), id.as_str()));
            }
            for (session, id) in handled {
                tx.execute(
                    "DELETE FROM attachments WHERE session_id=?1 AND id=?2",
                    params![session, id],
                )?;
            }
            // The unreadable row's turn is unknown, so the record sits at the session's
            // latest turn start: rewinding that turn refuses. It is pruned once two newer
            // turns exist.
            for (session, boundary, metadata) in unreadable {
                if unreadable_sessions.contains(session) {
                    tx.execute(
                        "INSERT OR REPLACE INTO checkpoint_records(session_id,id,boundary_seq,metadata,preimage_hash) VALUES(?1,?2,?3,?4,NULL)",
                        params![session, UNREADABLE_RECORD_ID, boundary, metadata],
                    )?;
                }
            }
            for (session, patch) in high_water {
                let key = Self::checkpoint_high_water_key(&session);
                let current = tx
                    .query_row("SELECT value FROM metadata WHERE key=?1", [&key], |row| {
                        row.get::<_, String>(0)
                    })
                    .optional()?
                    .map(|value| value.parse::<u64>())
                    .transpose()?
                    .unwrap_or(0);
                if patch > current {
                    tx.execute(
                        "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
                        params![key, patch.to_string()],
                    )?;
                }
            }
            tx.execute(
                "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
                params![marker.0, marker.1],
            )?;
            Ok(notes)
        })
    }

    /// One page of a session's checkpoint records as (id, metadata), in id order
    /// after `after`.
    pub fn checkpoint_records_page(
        &self,
        session_id: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.checkpoint_records_page(session_id, after, limit);
        }
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id,metadata FROM checkpoint_records WHERE session_id=?1 AND (?2 IS NULL OR id>?2) ORDER BY id LIMIT ?3",
            )?;
            Ok(stmt
                .query_map(params![session_id, after, limit as i64], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Preimage bytes of one checkpoint record, or None when no such record exists.
    pub fn checkpoint_preimage(
        &self,
        session_id: &str,
        record_id: &str,
    ) -> Result<Option<Vec<u8>>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.checkpoint_preimage(session_id, record_id);
        }
        self.read(|conn| {
            // A missing record and an unrestorable record (NULL preimage) both give None.
            let hash: Option<Option<String>> = conn
                .query_row(
                    "SELECT preimage_hash FROM checkpoint_records WHERE session_id=?1 AND id=?2",
                    params![session_id, record_id],
                    |row| row.get(0),
                )
                .optional()?;
            hash.flatten()
                .map(|hash| Self::checkpoint_get(conn, &hash))
                .transpose()
        })
    }
}
/// Attachment ID prefix that earlier builds gave checkpoint rows. Current builds
/// keep checkpoints in `checkpoint_records`; the one-time sweep deletes the old rows.
pub const CHECKPOINT_PREFIX: &str = "file-checkpoint-v1/";

/// A legacy checkpoint row that the sweep could not migrate cleanly, with the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyCheckpointNote {
    pub session_id: String,
    pub id: String,
    /// True when the row stays in `attachments`; false when it was migrated as unrestorable.
    pub left: bool,
    pub reason: String,
}

/// One checkpoint record for `Store::write_checkpoint`. `boundary_seq` is the
/// canonical sequence of the message that owns the change, and it decides pruning.
/// `patch_index` numbers the patch within its session and must not go below the
/// session's high-water mark. `metadata` is opaque to the core.
#[derive(Clone, Copy, Debug)]
pub struct CheckpointRecord<'a> {
    pub id: &'a str,
    pub patch_index: u64,
    pub boundary_seq: i64,
    pub metadata: &'a str,
}

/// Checkpoint tables, created with a fresh chat. `checkpoint_blobs` holds
/// zstd-compressed preimages keyed by their plain SHA-256; `checkpoint_records`
/// holds one row per checkpoint and names its preimage. A NULL `preimage_hash` marks
/// an unrestorable record, which has no preimage. The index serves the reference
/// check made when records are pruned.
pub(crate) const CHECKPOINT_SCHEMA: &str = "
CREATE TABLE checkpoint_blobs(hash TEXT PRIMARY KEY,raw_bytes INTEGER NOT NULL,data BLOB NOT NULL);
CREATE TABLE checkpoint_records(session_id TEXT NOT NULL REFERENCES sessions(id),id TEXT NOT NULL,boundary_seq INTEGER NOT NULL,metadata TEXT NOT NULL,preimage_hash TEXT REFERENCES checkpoint_blobs(hash),PRIMARY KEY(session_id,id));
CREATE INDEX checkpoint_records_preimage ON checkpoint_records(preimage_hash);
CREATE INDEX checkpoint_records_boundary ON checkpoint_records(session_id,boundary_seq);
";

/// Record ID of the unrestorable record the sweep writes for a session that has a
/// legacy row whose metadata cannot place it in a turn.
pub(crate) const UNREADABLE_RECORD_ID: &str = "unrestorable/legacy";
