//! Context facts live in the canonical chat, including when serving is disposable.
use super::*;

#[cfg(test)]
#[path = "store_context_event_tests.rs"]
mod tests;

impl Store {
    /// Journal the notification with its authoritative fact, preserving the owning
    /// write's existing telemetry and serving-buffer behavior.
    pub(super) fn write_context_events<T>(
        &self,
        session_id: &str,
        events: &[ContextEvent],
        context: bool,
        write: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        if events.is_empty() {
            return if context {
                self.write_context(write)
            } else {
                self.write(write)
            };
        }
        let store = self.host().unwrap_or_else(|| self.clone());
        let runtime = store.runtime();
        let write = |tx: &Connection| {
            let value = write(tx)?;
            let journal = events
                .iter()
                .map(|event| {
                    (
                        crate::EventKind::ContextCompaction {
                            event_id: event.id.clone(),
                            kind: event.kind,
                            trigger: event.trigger,
                        },
                        None,
                    )
                })
                .collect();
            Ok((value, journal))
        };
        if context {
            runtime.log.commit_context(&store, session_id, write)
        } else {
            runtime.log.commit(&store, session_id, "", None, write)
        }
    }

    /// Stage the disposable view before committing its canonical fact. Ordinary
    /// errors restore only this view; concurrent unrelated projections stay intact.
    /// The two WAL files still have no cross-file crash-atomicity promise.
    pub(super) fn write_context_view_events(
        &self,
        view: &ContextView,
        events: &[ContextEvent],
        write: impl FnOnce(&Connection) -> Result<String>,
    ) -> Result<()> {
        let host = self.host();
        let mut projection = None;
        let result = self.write_context_events(&view.session_id, events, false, |tx| {
            let data = write(tx)?;
            let stage = |conn: &Connection| -> Result<Option<String>> {
                let previous: Option<String> = conn.query_row(
                    "SELECT data FROM context_views WHERE session_id=?1", [&view.session_id], |row| row.get(0),
                ).optional()?;
                anyhow::ensure!(events.is_empty() || previous.as_deref() != Some(data.as_str()), "Applied view must change the persisted view");
                conn.execute("INSERT INTO context_views(session_id,data) VALUES(?1,?2) ON CONFLICT(session_id) DO UPDATE SET data=excluded.data", params![view.session_id, data])?;
                Ok(previous)
            };
            if let Some(host) = &host {
                let previous = host.write(stage)?;
                projection = Some((previous, data));
            } else {
                stage(tx)?;
            }
            Ok(())
        });
        if result.is_err()
            && let (Some(host), Some((previous, staged))) = (host, projection)
        {
            let restored = host.write(|tx| {
                if let Some(previous) = previous {
                    tx.execute(
                        "UPDATE context_views SET data=?3 WHERE session_id=?1 AND data=?2",
                        params![view.session_id, staged, previous],
                    )?;
                } else {
                    tx.execute(
                        "DELETE FROM context_views WHERE session_id=?1 AND data=?2",
                        params![view.session_id, staged],
                    )?;
                }
                Ok(())
            });
            if let Err(cleanup) = restored {
                return result.context(format!(
                    "Restoring rejected serving view also failed: {cleanup:#}"
                ));
            }
        }
        result
    }

    pub(super) fn migrate_context_facts(conn: &mut Connection, version: i64) -> Result<()> {
        if version >= 12 {
            return Ok(());
        }
        let tx = conn.transaction()?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS context_events(
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                kind TEXT NOT NULL CHECK(kind IN ('episode_written','chapter_closed','applied')),
                data TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS context_events_session ON context_events(session_id);",
        )?;
        let has_link = tx
            .prepare("PRAGMA table_info(request_usage)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|column| column == "message_seq");
        if !has_link {
            tx.execute_batch("ALTER TABLE request_usage ADD COLUMN message_seq INTEGER;")?;
        }
        tx.execute_batch(
            "CREATE INDEX IF NOT EXISTS request_usage_message ON request_usage(session_id,message_seq)
                WHERE message_seq IS NOT NULL;
             PRAGMA user_version=12;",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Return committed context facts in insertion order, independent of clock ties.
    pub fn context_events(&self, session_id: &str) -> Result<Vec<ContextEvent>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.context_events(session_id);
        }
        self.read(|conn| {
            let mut statement =
                conn.prepare("SELECT data FROM context_events WHERE session_id=?1 ORDER BY rowid")?;
            statement
                .query_map([session_id], |row| row.get::<_, String>(0))?
                .map(|row| Ok(serde_json::from_str(&row?)?))
                .collect()
        })
    }

    pub(super) fn insert_context_events_tx(
        tx: &Connection,
        session_id: &str,
        events: &[ContextEvent],
    ) -> Result<()> {
        for event in events {
            anyhow::ensure!(
                !event.id.trim().is_empty() && !event.created_at.trim().is_empty(),
                "Context event identity and creation time must be nonempty"
            );
            anyhow::ensure!(
                event.session_id == session_id && !session_id.trim().is_empty(),
                "Context event belongs to another session"
            );
            anyhow::ensure!(
                event.trigger_text.chars().count() <= 160,
                "Context event trigger text exceeds 160 characters"
            );
            let first = Self::session_message_seq(tx, session_id, &event.first_message_id)?;
            let last = Self::session_message_seq(tx, session_id, &event.last_message_id)?;
            anyhow::ensure!(first <= last, "Context event range cannot regress");
            let mut seen = std::collections::HashSet::new();
            for episode in &event.episode_ids {
                anyhow::ensure!(seen.insert(episode), "Duplicate context event episode");
                let member: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM context_episodes WHERE session_id=?1 AND id=?2)",
                    params![session_id, episode],
                    |row| row.get(0),
                )?;
                anyhow::ensure!(member, "Context event episode does not belong to session");
            }
            if let Some(summary) = &event.summary_id {
                let member: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM context_episodes WHERE session_id=?1 AND id=?2)
                        OR EXISTS(SELECT 1 FROM context_subject_chapters WHERE session_id=?1 AND id=?2)
                        OR EXISTS(SELECT 1 FROM context_chapters WHERE session_id=?1 AND id=?2)
                        OR EXISTS(SELECT 1 FROM context_checkpoints WHERE session_id=?1 AND id=?2)
                        OR EXISTS(SELECT 1 FROM context_checkpoint_versions WHERE session_id=?1 AND id=?2)",
                    params![session_id, summary],
                    |row| row.get(0),
                )?;
                anyhow::ensure!(member, "Context event summary does not belong to session");
            }
            let kind = serde_json::to_value(event.kind)?;
            tx.execute(
                "INSERT INTO context_events(id,session_id,kind,data) VALUES(?1,?2,?3,?4)",
                params![
                    event.id,
                    session_id,
                    kind.as_str().unwrap(),
                    serde_json::to_string(event)?
                ],
            )?;
        }
        Ok(())
    }
}
