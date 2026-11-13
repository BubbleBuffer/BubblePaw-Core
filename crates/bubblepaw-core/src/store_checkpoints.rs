//! File checkpoint records.
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
/// Attachment ID prefix reserved for checkpoint rows of earlier builds. Current
/// builds keep checkpoints in `checkpoint_records`; attachments may not use it.
pub const CHECKPOINT_PREFIX: &str = "file-checkpoint-v1/";

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
