use super::*;
use crate::{Reminder, ReminderMetadata};
use std::collections::HashSet;

const MESSAGE_CUTOFF: &str = "reminders_v1_message_cutoff";
const INPUT_CUTOFF: &str = "reminders_v1_input_cutoff";

impl Store {
    pub(super) fn initialize_reminders(connection: &Connection) -> Result<()> {
        let tx = connection.unchecked_transaction()?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS reminder_metadata(
                session_id TEXT NOT NULL REFERENCES sessions(id),
                record_id TEXT NOT NULL,
                data TEXT NOT NULL,
                PRIMARY KEY(session_id,record_id)
            );",
        )?;
        // These boundaries describe the first additive migration, never the
        // current open. Absence of metadata on newer records is not legacy.
        for (key, table) in [(MESSAGE_CUTOFF, "messages"), (INPUT_CUTOFF, "inputs")] {
            tx.execute(
                &format!("INSERT OR IGNORE INTO metadata(key,value) SELECT ?1,CAST(COALESCE(MAX(seq),0) AS TEXT) FROM {table}"),
                [key],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Accept generated reminder text and its identity in one transaction.
    pub fn queue_reminder(
        &self,
        session: &str,
        kind: InputKind,
        reminder: Reminder,
    ) -> Result<QueuedInput> {
        if let Some(store) = self.scoped(session)? {
            return store.queue_reminder(session, kind, reminder);
        }
        self.session(session)?;
        let input = QueuedInput {
            id: id(),
            session_id: session.into(),
            kind,
            text: reminder.text.clone(),
            state: "pending".into(),
            origin: Some(MessageOrigin::Generated),
        };
        let metadata = ReminderMetadata::new("root", vec![reminder]);
        self.write(|tx| {
            Self::queue_tx(tx, &input)?;
            Self::write_reminder_metadata_tx(tx, session, &input.id, &metadata)
        })?;
        Ok(input)
    }

    pub fn reminder_metadata(
        &self,
        session: &str,
        record_id: &str,
    ) -> Result<Option<ReminderMetadata>> {
        if let Some(store) = self.scoped(session)? {
            return store.reminder_metadata(session, record_id);
        }
        self.read(|conn| Self::reminder_metadata_tx(conn, session, record_id))
    }

    pub(crate) fn reminder_metadata_tx(
        conn: &Connection,
        session: &str,
        record_id: &str,
    ) -> Result<Option<ReminderMetadata>> {
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM reminder_metadata WHERE session_id=?1 AND record_id=?2",
                params![session, record_id],
                |row| row.get(0),
            )
            .optional()?;
        data.map(|value| {
            let metadata: ReminderMetadata = serde_json::from_str(&value)?;
            anyhow::ensure!(
                metadata.version == 1,
                "Unsupported reminder metadata version"
            );
            Ok(metadata)
        })
        .transpose()
    }

    pub(crate) fn write_reminder_metadata_tx(
        conn: &Connection,
        session: &str,
        record_id: &str,
        metadata: &ReminderMetadata,
    ) -> Result<()> {
        anyhow::ensure!(
            metadata.version == 1,
            "Unsupported reminder metadata version"
        );
        let data = serde_json::to_string(metadata)?;
        conn.execute(
            "INSERT INTO reminder_metadata(session_id,record_id,data) VALUES(?1,?2,?3) ON CONFLICT(session_id,record_id) DO NOTHING",
            params![session, record_id, data],
        )?;
        let stored: String = conn.query_row(
            "SELECT data FROM reminder_metadata WHERE session_id=?1 AND record_id=?2",
            params![session, record_id],
            |row| row.get(0),
        )?;
        anyhow::ensure!(stored == data, "Reminder metadata is immutable");
        Ok(())
    }

    /// Metadata for a generated input. Inputs queued without metadata get a
    /// per-input kind; those older than the migration cutoff also keep their
    /// original text as a legacy source. `write` persists synthesized metadata.
    pub(crate) fn input_reminder_metadata_tx(
        conn: &Connection,
        session: &str,
        input: &str,
        text: &str,
        write: bool,
    ) -> Result<ReminderMetadata> {
        if let Some(metadata) = Self::reminder_metadata_tx(conn, session, input)? {
            return Ok(metadata);
        }
        let legacy: bool = conn.query_row(
            "SELECT seq<=CAST((SELECT value FROM metadata WHERE key=?2) AS INTEGER) FROM inputs WHERE id=?1",
            params![input, INPUT_CUTOFF],
            |row| row.get(0),
        )?;
        let prefix = if legacy {
            "legacy_input"
        } else {
            "generated_input"
        };
        let reminder = Reminder::new(format!("{prefix}:{input}"), text);
        let mut metadata = ReminderMetadata::new("root", vec![reminder]);
        if legacy {
            metadata.legacy_sources.push(crate::LegacyReminderSource {
                record_id: input.into(),
                text: text.into(),
            });
        }
        if write {
            Self::write_reminder_metadata_tx(conn, session, input, &metadata)?;
        }
        Ok(metadata)
    }

    pub(crate) fn copy_reminder_metadata_tx(
        conn: &Connection,
        source_session: &str,
        source_id: &str,
        destination_session: &str,
        destination_id: &str,
    ) -> Result<()> {
        if let Some(metadata) = Self::reminder_metadata_tx(conn, source_session, source_id)? {
            Self::write_reminder_metadata_tx(conn, destination_session, destination_id, &metadata)?;
        }
        Ok(())
    }

    /// Only records already present at the first migration may use the legacy
    /// reminder parser. Missing newer metadata must remain ordinary content.
    pub fn reminder_legacy_eligible(&self, session: &str, record_id: &str) -> Result<bool> {
        if let Some(store) = self.scoped(session)? {
            return store.reminder_legacy_eligible(session, record_id);
        }
        let (eligible, archived_cutoff) = self.read(|conn| {
            let annotated: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM reminder_metadata WHERE session_id=?1 AND record_id=?2)",
                params![session, record_id],
                |row| row.get(0),
            )?;
            if annotated {
                return Ok((false, None));
            }
            let eligible = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE session_id=?1 AND message_id=?2 AND seq<=CAST((SELECT value FROM metadata WHERE key=?3) AS INTEGER))
                OR EXISTS(SELECT 1 FROM inputs WHERE session_id=?1 AND id=?2 AND seq<=CAST((SELECT value FROM metadata WHERE key=?4) AS INTEGER))",
                params![session, record_id, MESSAGE_CUTOFF, INPUT_CUTOFF],
                |row| row.get::<_, bool>(0),
            )?;
            let cutoff = conn.query_row(
                "SELECT CAST(value AS INTEGER) FROM metadata WHERE key=?1",
                [INPUT_CUTOFF],
                |row| row.get::<_, i64>(0),
            )?;
            Ok((eligible, Some(cutoff)))
        })?;
        if eligible {
            return Ok(true);
        }
        // Resolved input rows are disposable serving records, but source_seq
        // retains their original canonical position across that move.
        if let (Some(host), Some(cutoff)) = (self.host(), archived_cutoff) {
            return host.read(|conn| {
                Ok(conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM inputs WHERE session_id=?1 AND id=?2 AND source_seq<=?3)",
                    params![session, record_id, cutoff],
                    |row| row.get(0),
                )?)
            });
        }
        Ok(false)
    }

    pub(crate) fn pending_reminder_inputs(
        &self,
        session: &str,
        request_kind: &str,
    ) -> Result<Vec<(String, Reminder)>> {
        if let Some(store) = self.scoped(session)? {
            return store.pending_reminder_inputs(session, request_kind);
        }
        let generated = serde_json::to_string(&MessageOrigin::Generated)?;
        self.read(|conn| {
            // Staged inputs, plus generated steers that wait for this flush.
            let mut statement = conn.prepare(
                "SELECT id,text,content_blob,content_format FROM inputs WHERE session_id=?1 AND (state='reminder_pending' OR (state='pending' AND kind='steer' AND origin IS ?2)) ORDER BY seq",
            )?;
            let rows = statement.query_map(params![session, generated], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?, row.get::<_, Option<String>>(3)?))
            })?;
            let mut inputs = Vec::new();
            for row in rows {
                let (input, text, blob, format) = row?;
                let text = Self::input_text(conn, &text, blob.as_deref(), format.as_deref())?;
                let metadata = Self::input_reminder_metadata_tx(conn, session, &input, &text, false)?;
                anyhow::ensure!(metadata.reminders.len() == 1, "Input reminder metadata must contain one reminder");
                if metadata.request_kind == request_kind {
                    let record = metadata.reminders.into_iter().next().unwrap();
                    anyhow::ensure!(crate::fingerprint(text.as_bytes()) == record.text_sha256,
                        "Reminder input text does not match metadata");
                    inputs.push((input, Reminder { kind: record.kind, text, deliveries: record.deliveries }));
                }
            }
            Ok(inputs)
        })
    }

    /// Restore one latest sent value per kind for this request lineage.
    pub fn last_reminders(
        &self,
        session: &str,
        request_kind: &str,
    ) -> Result<Vec<crate::ReminderRecord>> {
        if let Some(store) = self.scoped(session)? {
            return store.last_reminders(session, request_kind);
        }
        self.read(|conn| {
            let mut statement = conn.prepare(
                "SELECT r.data FROM reminder_metadata r JOIN messages m ON m.session_id=r.session_id AND m.message_id=r.record_id WHERE r.session_id=?1 ORDER BY m.seq DESC",
            )?;
            let rows = statement.query_map([session], |row| row.get::<_, String>(0))?;
            let mut seen = HashSet::new();
            let mut reminders = Vec::new();
            for row in rows {
                let metadata: ReminderMetadata = serde_json::from_str(&row?)?;
                anyhow::ensure!(metadata.version == 1, "Unsupported reminder metadata version");
                if metadata.request_kind != request_kind {
                    continue;
                }
                for reminder in metadata.reminders.into_iter().rev() {
                    if seen.insert(reminder.kind.clone()) {
                        reminders.push(reminder);
                    }
                }
            }
            Ok(reminders)
        })
    }

    pub(crate) fn append_reminder_message(
        &self,
        message: &Message,
        metadata: &ReminderMetadata,
        input_ids: &[String],
    ) -> Result<()> {
        if let Some(store) = self.scoped(&message.session_id)? {
            return store.append_reminder_message(message, metadata, input_ids);
        }
        self.write(|tx| {
            let mut metadata = metadata.clone();
            for input in input_ids {
                let (text, blob, format): (String, Option<String>, Option<String>) = tx.query_row(
                    "SELECT text,content_blob,content_format FROM inputs WHERE session_id=?1 AND id=?2",
                    params![message.session_id, input],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                let text = Self::input_text(tx, &text, blob.as_deref(), format.as_deref())?;
                let source =
                    Self::input_reminder_metadata_tx(tx, &message.session_id, input, &text, true)?;
                metadata.legacy_sources.extend(source.legacy_sources);
            }
            Self::insert_message(tx, message)?;
            Self::write_reminder_metadata_tx(tx, &message.session_id, &message.id, &metadata)?;
            Self::acknowledge_reminder_inputs_tx(
                tx,
                &message.session_id,
                input_ids,
                Some(&message.id),
            )
        })
    }

    pub(crate) fn acknowledge_reminder_inputs(
        &self,
        session: &str,
        input_ids: &[String],
    ) -> Result<()> {
        if input_ids.is_empty() {
            return Ok(());
        }
        if let Some(store) = self.scoped(session)? {
            return store.acknowledge_reminder_inputs(session, input_ids);
        }
        self.write(|tx| Self::acknowledge_reminder_inputs_tx(tx, session, input_ids, None))
    }

    fn acknowledge_reminder_inputs_tx(
        tx: &Connection,
        session: &str,
        input_ids: &[String],
        message_id: Option<&str>,
    ) -> Result<()> {
        for input in input_ids {
            let changed = tx.execute(
                "UPDATE inputs SET state='consumed',text='',content_blob=NULL,content_format=NULL,message_id=?3 WHERE session_id=?1 AND id=?2 AND state IN ('reminder_pending','pending')",
                params![session, input, message_id],
            )?;
            anyhow::ensure!(
                changed == 1,
                "Reminder input is not pending in this session"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reminder(text: &str) -> Reminder {
        Reminder {
            kind: "test".into(),
            text: text.into(),
            deliveries: Vec::new(),
        }
    }

    #[test]
    fn queue_rolls_back_accepted_text_if_metadata_fails() -> Result<()> {
        let store = Store::in_memory()?;
        let session = store.create_session("queue atomic")?;
        store.read(|conn| {
            conn.execute_batch("CREATE TRIGGER fail_reminder BEFORE INSERT ON reminder_metadata BEGIN SELECT RAISE(ABORT,'injected metadata failure'); END;")?;
            Ok(())
        })?;
        let before = store.storage_stats()?.chat_transaction_count;
        assert!(
            store
                .queue_reminder(&session.id, InputKind::Steer, reminder("accepted raw"))
                .is_err()
        );
        assert!(store.queued_inputs(&session.id)?.is_empty());
        assert_eq!(store.storage_stats()?.chat_transaction_count, before);
        Ok(())
    }

    #[test]
    fn append_and_acknowledgement_roll_back_together() -> Result<()> {
        let store = Store::in_memory()?;
        let session = store.create_session("atomic")?;
        let input = store.queue_reminder(&session.id, InputKind::Steer, reminder("raw"))?;
        store.write(|tx| {
            tx.execute(
                "UPDATE inputs SET state='reminder_pending' WHERE id=?1",
                [&input.id],
            )?;
            Ok(())
        })?;
        let message = Store::message(&session.id, Role::User, "rendered".into());
        let metadata = ReminderMetadata::new("root", vec![reminder("raw")]);
        let before = store.storage_stats()?.chat_transaction_count;
        assert!(
            store
                .append_reminder_message(&message, &metadata, &[input.id.clone(), "missing".into()])
                .is_err()
        );
        assert!(store.messages(&session.id)?.is_empty());
        assert!(store.reminder_metadata(&session.id, &message.id)?.is_none());
        assert_eq!(store.pending_reminder_inputs(&session.id, "root")?.len(), 1);
        assert_eq!(store.storage_stats()?.chat_transaction_count, before);
        store.append_reminder_message(&message, &metadata, std::slice::from_ref(&input.id))?;
        assert!(
            store
                .pending_reminder_inputs(&session.id, "root")?
                .is_empty()
        );
        assert_eq!(
            store.last_reminders(&session.id, "root")?[0].text_sha256,
            crate::fingerprint(b"raw")
        );
        assert_eq!(store.storage_stats()?.chat_transaction_count, before + 1);
        assert_eq!(store.queued_inputs(&session.id)?[0].state, "consumed");
        Ok(())
    }

    #[test]
    fn duplicate_acknowledgement_is_atomic_and_empty_is_free() -> Result<()> {
        let store = Store::in_memory()?;
        let session = store.create_session("dedup")?;
        let input = store.queue_reminder(&session.id, InputKind::Steer, reminder("same"))?;
        store.write(|tx| {
            tx.execute(
                "UPDATE inputs SET state='reminder_pending' WHERE id=?1",
                [&input.id],
            )?;
            Ok(())
        })?;
        let before = store.storage_stats()?.chat_transaction_count;
        store.acknowledge_reminder_inputs(&session.id, &[])?;
        assert_eq!(store.storage_stats()?.chat_transaction_count, before);
        assert!(
            store
                .acknowledge_reminder_inputs(&session.id, &[input.id.clone(), "missing".into()])
                .is_err()
        );
        assert_eq!(store.pending_reminder_inputs(&session.id, "root")?.len(), 1);
        store.acknowledge_reminder_inputs(&session.id, &[input.id])?;
        assert!(
            store
                .pending_reminder_inputs(&session.id, "root")?
                .is_empty()
        );
        assert!(store.messages(&session.id)?.is_empty());
        assert_eq!(store.storage_stats()?.chat_transaction_count, before + 1);
        Ok(())
    }
}
