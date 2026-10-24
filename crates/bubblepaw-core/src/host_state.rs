//! Bounded host-owned state in the authoritative chat, independent of tools.
use crate::{EventKind, Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS host_state(session_id TEXT NOT NULL REFERENCES sessions(id),key TEXT NOT NULL,value TEXT NOT NULL,PRIMARY KEY(session_id,key));";
const MAX_STATE_BYTES: usize = 64 * 1024;

#[derive(Deserialize, Serialize)]
struct Snapshot {
    // A state change occurs after this canonical message. Selecting the owning
    // assistant message therefore excludes its tools; selecting their complete
    // result includes them, even when nested tools discard their return values.
    boundary_seq: i64,
}

fn snapshot_prefix(key: &str) -> String {
    // Keys remain arbitrary UTF-8 without prefix collisions between keys.
    let key: String = key.bytes().map(|byte| format!("{byte:02x}")).collect();
    format!("host-state-v1/{key}/")
}

fn message_sequences(connection: &Connection, session: &str, cutoff: i64) -> Result<Vec<i64>> {
    let mut statement = connection
        .prepare("SELECT seq FROM messages WHERE session_id=?1 AND seq<=?2 ORDER BY seq")?;
    Ok(statement
        .query_map(params![session, cutoff], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

impl Store {
    pub fn host_state(&self, session_id: &str, key: &str) -> Result<Option<Value>> {
        if let Some(chat) = self.scoped(session_id)? {
            return chat.host_state(session_id, key);
        }
        self.read(|connection| {
            let value: Option<String> = connection
                .query_row(
                    "SELECT value FROM host_state WHERE session_id=?1 AND key=?2",
                    params![session_id, key],
                    |row| row.get(0),
                )
                .optional()?;
            value
                .map(|value| serde_json::from_str(&value).map_err(Into::into))
                .transpose()
        })
    }

    /// Update one value and publish its event under the same snapshot lock.
    /// A failed validation leaves both the stored value and event stream intact.
    pub fn update_host_state(
        &self,
        session_id: &str,
        run_id: &str,
        key: &str,
        update: impl FnOnce(Option<Value>) -> Result<Value>,
    ) -> Result<Value> {
        self.update_host_state_inner(session_id, run_id, key, false, update)
    }

    /// Opt in to historical state at canonical message boundaries. The immutable
    /// snapshot attachment/blob, latest value and event share one transaction.
    /// Multiple changes inside one tool cell retain their commit order.
    pub fn update_host_state_snapshot(
        &self,
        session_id: &str,
        run_id: &str,
        key: &str,
        update: impl FnOnce(Option<Value>) -> Result<Value>,
    ) -> Result<Value> {
        self.update_host_state_inner(session_id, run_id, key, true, update)
    }

    fn update_host_state_inner(
        &self,
        session_id: &str,
        run_id: &str,
        key: &str,
        snapshot: bool,
        update: impl FnOnce(Option<Value>) -> Result<Value>,
    ) -> Result<Value> {
        ensure!(
            !key.is_empty() && key.len() <= 64,
            "Host state key must be 1..64 bytes"
        );
        self.runtime()
            .log
            .commit(self, session_id, run_id, None, |tx| {
                let previous: Option<String> = tx
                    .query_row(
                        "SELECT value FROM host_state WHERE session_id=?1 AND key=?2",
                        params![session_id, key],
                        |row| row.get(0),
                    )
                    .optional()?;
                let previous = previous
                    .map(|value| serde_json::from_str(&value))
                    .transpose()?;
                let value = update(previous)?;
                Self::write_host_state_tx(tx, session_id, key, &value, snapshot)?;
                Ok((
                    value.clone(),
                    vec![(
                        EventKind::HostStateChanged {
                            key: key.into(),
                            value,
                        },
                        None,
                    )],
                ))
            })
    }

    /// Caller owns the transaction; snapshot and current value commit together.
    pub(crate) fn write_host_state_tx(
        tx: &Connection,
        session_id: &str,
        key: &str,
        value: &Value,
        snapshot: bool,
    ) -> Result<()> {
        let encoded = serde_json::to_string(&value)?;
        ensure!(
            encoded.len() <= MAX_STATE_BYTES,
            "Host state exceeds 64 KiB"
        );
        if snapshot {
            let prefix = snapshot_prefix(key);
            let previous: Option<String> = tx.query_row(
                "SELECT id FROM attachments WHERE session_id=?1 AND substr(id,1,length(?2))=?2 ORDER BY id DESC LIMIT 1",
                params![session_id, prefix], |row| row.get(0),
            ).optional()?;
            let ordinal = previous
                .map(|id| -> Result<u64> {
                    Ok(id
                        .rsplit('/')
                        .next()
                        .context("Invalid state snapshot ID")?
                        .parse::<u64>()?)
                })
                .transpose()?
                .unwrap_or(0)
                .checked_add(1)
                .context("State snapshot ordinal exhausted")?;
            let boundary_seq = tx.query_row(
                "SELECT coalesce(max(seq),0) FROM messages WHERE session_id=?1",
                [session_id],
                |row| row.get(0),
            )?;
            let metadata = serde_json::to_string(&Snapshot { boundary_seq })?;
            let hash = Self::blob_put(tx, encoded.as_bytes())?;
            tx.execute(
                "INSERT INTO attachments(session_id,id,metadata,blob_hash) VALUES(?1,?2,?3,?4)",
                params![session_id, format!("{prefix}{ordinal:020}"), metadata, hash],
            )?;
        }
        tx.execute(
            "INSERT INTO host_state(session_id,key,value) VALUES(?1,?2,?3) ON CONFLICT(session_id,key) DO UPDATE SET value=excluded.value",
            params![session_id, key, encoded],
        )?;
        Ok(())
    }

    /// Copy one opted-in key into a fresh fork of the source's canonical prefix.
    /// Call from `fork_at_with` so source admission stays excluded. History is
    /// copied with remapped local message sequences; neither chat references
    /// mutable state in the other. Unsnapshotted keys are deliberately omitted.
    /// The fork must have no existing value or snapshots for this key.
    pub fn copy_host_state_at(
        &self,
        source: &str,
        message_id: &str,
        fork: &str,
        key: &str,
    ) -> Result<Option<Value>> {
        ensure!(
            source != fork,
            "State snapshot destination must be a different session"
        );
        let cutoff = self.message_seq(source, message_id)?;
        let prefix = snapshot_prefix(key);
        let source_store = self.scoped(source)?.unwrap_or_else(|| self.clone());
        let (sequences, snapshots) = source_store.read(|connection| {
            let sequences = message_sequences(connection, source, cutoff)?;
            let mut statement = connection.prepare(
                "SELECT id,metadata,blob_hash FROM attachments WHERE session_id=?1 AND substr(id,1,length(?2))=?2 ORDER BY id",
            )?;
            let rows = statement.query_map(params![source, prefix], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })?;
            let mut snapshots = Vec::new();
            for row in rows {
                let (id, metadata, hash) = row?;
                let snapshot: Snapshot = serde_json::from_str(&metadata)?;
                if snapshot.boundary_seq < cutoff {
                    let boundary = if snapshot.boundary_seq == 0 { None } else {
                        Some(sequences.binary_search(&snapshot.boundary_seq)
                            .map_err(|_| anyhow::anyhow!("State snapshot boundary is missing"))?)
                    };
                    snapshots.push((id, boundary, Self::blob_get(connection, &hash)?));
                }
            }
            Ok((sequences, snapshots))
        })?;
        if snapshots.is_empty() {
            return Ok(None);
        }
        self.runtime().log.commit(self, fork, "", None, |tx| {
            let occupied: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM host_state WHERE session_id=?1 AND key=?2) OR EXISTS(SELECT 1 FROM attachments WHERE session_id=?1 AND substr(id,1,length(?3))=?3)",
                params![fork, key, prefix], |row| row.get(0),
            )?;
            ensure!(!occupied, "State snapshot destination is not empty");
            let fork_sequences = message_sequences(tx, fork, i64::MAX)?;
            ensure!(fork_sequences.len() == sequences.len(), "State snapshot destination must be the selected canonical prefix");
            let mut latest = None;
            for (id, boundary, bytes) in snapshots {
                let boundary_seq = boundary.map_or(0, |index| fork_sequences[index]);
                let metadata = serde_json::to_string(&Snapshot { boundary_seq })?;
                let hash = Self::blob_put(tx, &bytes)?;
                tx.execute(
                    "INSERT INTO attachments(session_id,id,metadata,blob_hash) VALUES(?1,?2,?3,?4)",
                    params![fork, id, metadata, hash],
                )?;
                latest = Some(serde_json::from_slice::<Value>(&bytes)?);
            }
            let value = latest.context("State snapshot history is empty")?;
            tx.execute("INSERT INTO host_state(session_id,key,value) VALUES(?1,?2,?3)",
                params![fork, key, serde_json::to_string(&value)?])?;
            Ok((Some(value.clone()), vec![(EventKind::HostStateChanged { key: key.into(), value }, None)]))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn host_state_replaces_atomically_and_is_deleted_with_session() -> Result<()> {
        let store = Store::in_memory()?;
        let session = store.create_session("host state")?;
        store.update_host_state(&session.id, "", "example", |_| Ok(json!([1])))?;
        store.update_host_state(&session.id, "", "example", |previous| {
            assert_eq!(previous, Some(json!([1])));
            Ok(json!([2]))
        })?;
        assert!(
            store
                .update_host_state(&session.id, "", "example", |_| Ok(json!(
                    "x".repeat(MAX_STATE_BYTES)
                )))
                .is_err()
        );
        assert_eq!(store.host_state(&session.id, "example")?, Some(json!([2])));
        store.delete_session(&session.id)?;
        Ok(())
    }

    #[test]
    fn snapshots_keep_order_roll_back_failures_and_remap_fork_boundaries() -> Result<()> {
        use crate::Role;

        let store = Store::in_memory()?;
        let source = store.create_session("snapshot source")?.id;
        let other = store.create_session("interleaved sequences")?.id;
        let first = Store::message(&source, Role::User, "before changes".into());
        store.append_batch(&[first])?;
        store.update_host_state_snapshot(&source, "", "example", |_| Ok(json!([1])))?;
        store.update_host_state_snapshot(&source, "", "example", |_| Ok(json!([2])))?;
        store.append_batch(&[Store::message(&other, Role::User, "sequence gap".into())])?;
        let target = Store::message(&source, Role::Assistant, "changes completed".into());
        store.append_batch(std::slice::from_ref(&target))?;
        store.update_host_state_snapshot(&source, "", "example", |_| Ok(json!([3])))?;
        store.update_host_state(&source, "", "ephemeral", |_| Ok(json!({"pending":true})))?;
        let before = store.storage_stats()?.transaction_count;
        assert!(
            store
                .update_host_state_snapshot(&source, "", "example", |_| {
                    Ok(json!("x".repeat(MAX_STATE_BYTES)))
                })
                .is_err()
        );
        assert_eq!(store.storage_stats()?.transaction_count, before);
        assert_eq!(
            store
                .attachment_metadata_page(&source, &snapshot_prefix("example"), None, 100)?
                .len(),
            3
        );
        assert_eq!(store.host_state(&source, "example")?, Some(json!([3])));

        let fork = store.fork_at_with(&source, &target.id, "snapshot fork", |fork| {
            assert_eq!(
                store.copy_host_state_at(&source, &target.id, &fork.id, "example")?,
                Some(json!([2]))
            );
            assert_eq!(
                store.copy_host_state_at(&source, &target.id, &fork.id, "ephemeral")?,
                None
            );
            Ok(())
        })?;
        assert_eq!(store.host_state(&fork.id, "example")?, Some(json!([2])));
        assert_eq!(store.host_state(&fork.id, "ephemeral")?, None);
        let copied = store.messages(&fork.id)?;
        let repeated = store.fork_at_with(&fork.id, &copied[1].id, "repeated", |next| {
            store.copy_host_state_at(&fork.id, &copied[1].id, &next.id, "example")?;
            Ok(())
        })?;
        assert_eq!(store.host_state(&repeated.id, "example")?, Some(json!([2])));
        let earlier = store.fork_at_with(&fork.id, &copied[0].id, "before changes", |next| {
            store.copy_host_state_at(&fork.id, &copied[0].id, &next.id, "example")?;
            Ok(())
        })?;
        assert_eq!(store.host_state(&earlier.id, "example")?, None);
        store.update_host_state_snapshot(&fork.id, "", "example", |_| Ok(json!([4])))?;
        assert_eq!(store.host_state(&source, "example")?, Some(json!([3])));
        assert_eq!(store.host_state(&repeated.id, "example")?, Some(json!([2])));
        Ok(())
    }
}
