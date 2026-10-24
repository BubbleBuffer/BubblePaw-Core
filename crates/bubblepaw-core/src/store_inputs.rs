//! Accepted input storage and canonical consumption.
use super::*;

impl Store {
    pub fn queue(
        &self,
        session_id: &str,
        kind: InputKind,
        text: impl Into<String>,
    ) -> Result<QueuedInput> {
        self.queue_with_origin(session_id, kind, text, MessageOrigin::Typed)
    }
    pub fn queue_with_origin(
        &self,
        session_id: &str,
        kind: InputKind,
        text: impl Into<String>,
        origin: MessageOrigin,
    ) -> Result<QueuedInput> {
        if let Some(store) = self.scoped(session_id)? {
            return store.queue_with_origin(session_id, kind, text, origin);
        }
        self.session(session_id)?;
        let input = QueuedInput {
            id: id(),
            session_id: session_id.into(),
            kind,
            text: text.into(),
            state: "pending".into(),
            origin: Some(origin),
        };
        self.write(|tx| Self::queue_tx(tx, &input))?;
        Ok(input)
    }
    pub fn queued_inputs(&self, session_id: &str) -> Result<Vec<QueuedInput>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.queued_inputs(session_id);
        }
        self.read(|conn| {
            let mut inputs = Vec::new();
            if let Some(host) = self.host() {
                inputs.extend(host.read(|projection| {
                    Self::read_queued_inputs(conn, projection, session_id, "source_seq")
                })?);
            }
            inputs.extend(Self::read_queued_inputs(conn, conn, session_id, "seq")?);
            Ok(inputs)
        })
    }
    fn read_queued_inputs(
        conn: &Connection,
        operations: &Connection,
        session_id: &str,
        order: &str,
    ) -> Result<Vec<QueuedInput>> {
        let mut stmt = operations.prepare(&format!("SELECT id,kind,text,state,origin,content_blob,content_format FROM inputs WHERE session_id=?1 ORDER BY {order}"))?;
        stmt.query_map([session_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?
        .map(|row| {
            let (id, kind, text, state, origin, content_blob, content_format) = row?;
            let text = Self::input_text(
                conn,
                &text,
                content_blob.as_deref(),
                content_format.as_deref(),
            )?;
            Ok(QueuedInput {
                id,
                session_id: session_id.into(),
                kind: if kind == "steer" {
                    InputKind::Steer
                } else {
                    InputKind::Followup
                },
                text,
                state,
                origin: origin
                    .map(|value| serde_json::from_str(&value))
                    .transpose()?,
            })
        })
        .collect()
    }
    pub(crate) fn consume(&self, session_id: &str, steers_only: bool, one: bool) -> Result<usize> {
        if let Some(store) = self.scoped(session_id)? {
            return store.consume(session_id, steers_only, one);
        }
        // Generated steers stay pending until the request build flushes them
        // as one reminder message, in the same transaction that consumes them.
        let generated = serde_json::to_string(&MessageOrigin::Generated)?;
        let (all, writable): (i64, i64) = {
            let conn = self.0.connection.lock().unwrap();
            conn.query_row("SELECT COUNT(*),COALESCE(SUM(CASE WHEN ?2=1 AND origin IS ?3 THEN 0 ELSE 1 END),0) FROM inputs WHERE session_id=?1 AND state='pending' AND (?2=0 OR kind='steer')",params![session_id,steers_only,generated],|r|Ok((r.get(0)?,r.get(1)?)))?
        };
        if writable == 0 {
            let all = usize::try_from(all).context("Input count exceeds address space")?;
            return Ok(if one { all.min(1) } else { all });
        }
        self.write(|tx| {
            Self::consume_origin_tx(tx, session_id, steers_only, one, None, steers_only)
        })
    }
    /// Drain one ordered steering batch at a main request's final snapshot.
    /// Return the exact canonical messages from the same consumption transaction.
    pub(crate) fn consume_steering_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.consume_steering_messages(session_id);
        }
        let generated = serde_json::to_string(&MessageOrigin::Generated)?;
        let pending = self.read(|conn| Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM inputs WHERE session_id=?1 AND state='pending' AND kind='steer' AND origin IS NOT ?2)",
            params![session_id, generated], |row| row.get::<_, bool>(0),
        )?))?;
        if !pending {
            return Ok(Vec::new());
        }
        self.write(|tx| Self::consume_messages_tx(tx, session_id, true, false, None, true))
    }
    pub(crate) fn has_pending(&self, session_id: &str) -> Result<bool> {
        if let Some(store) = self.scoped(session_id)? {
            return store.has_pending(session_id);
        }
        let conn = self.0.connection.lock().unwrap();
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM inputs WHERE session_id=?1 AND state='pending')",
            [session_id],
            |r| r.get(0),
        )?)
    }
    /// Host-generated steering records an input without asking a script-only
    /// run to start a model. Consume it after the script result is committed,
    /// preserving the assistant/call/result order and ordinary user followups.
    pub(crate) fn consume_generated_steering(&self, session_id: &str) -> Result<usize> {
        if let Some(store) = self.scoped(session_id)? {
            return store.consume_generated_steering(session_id);
        }
        let origin = serde_json::to_string(&MessageOrigin::Generated)?;
        let pending = self.read(|connection| Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM inputs WHERE session_id=?1 AND state='pending' AND kind='steer' AND origin=?2)",
            params![session_id, origin], |row| row.get::<_, bool>(0),
        )?))?;
        if !pending {
            return Ok(0);
        }
        self.write(|tx| {
            let origin = Some(MessageOrigin::Generated);
            Self::consume_origin_tx(tx, session_id, true, false, origin, false)
        })
    }

    pub(crate) fn consume_tx(
        tx: &Connection,
        session_id: &str,
        steers_only: bool,
        one: bool,
    ) -> Result<usize> {
        Self::consume_origin_tx(tx, session_id, steers_only, one, None, false)
    }
    fn consume_origin_tx(
        tx: &Connection,
        session_id: &str,
        steers_only: bool,
        one: bool,
        origin: Option<MessageOrigin>,
        defer_generated: bool,
    ) -> Result<usize> {
        let origin_json = origin
            .map(|value| serde_json::to_string(&value))
            .transpose()?;
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM inputs WHERE session_id=?1 AND state='pending' AND (?2=0 OR kind='steer') AND (?3 IS NULL OR origin=?3)",
            params![session_id,steers_only,origin_json], |row| row.get(0),
        )?;
        Self::consume_messages_tx(tx, session_id, steers_only, one, origin, defer_generated)?;
        usize::try_from(if one { count.min(1) } else { count })
            .context("Input count exceeds address space")
    }
    fn consume_messages_tx(
        tx: &Connection,
        session_id: &str,
        steers_only: bool,
        one: bool,
        origin: Option<MessageOrigin>,
        defer_generated: bool,
    ) -> Result<Vec<Message>> {
        let origin = origin
            .map(|origin| serde_json::to_string(&origin))
            .transpose()?;
        let values: Vec<InputContentRow> = {
            let mut statement = tx.prepare("SELECT id,text,origin,content_blob,content_format FROM inputs WHERE session_id=?1 AND state='pending' AND (?2=0 OR kind='steer') AND (?4 IS NULL OR origin=?4) ORDER BY seq LIMIT ?3")?;
            statement
                .query_map(
                    params![session_id, steers_only, if one { 1 } else { -1 }, origin],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut messages = Vec::with_capacity(values.len());
        for (input_id, text, origin, content_blob, content_format) in &values {
            let text =
                Self::input_text(tx, text, content_blob.as_deref(), content_format.as_deref())?;
            let origin = origin.as_deref().map(serde_json::from_str).transpose()?;
            if origin == Some(MessageOrigin::Generated) {
                if defer_generated {
                    continue;
                }
                Self::input_reminder_metadata_tx(tx, session_id, input_id, &text, true)?;
                tx.execute(
                    "UPDATE inputs SET state='reminder_pending' WHERE id=?1",
                    [input_id],
                )?;
                continue;
            }
            let mut message = Self::message(session_id, Role::User, text);
            message.origin = origin;
            Self::insert_message(tx, &message)?;
            tx.execute("UPDATE inputs SET state='consumed',text='',content_blob=NULL,content_format=NULL,message_id=?2 WHERE id=?1", params![input_id,message.id])?;
            messages.push(message);
        }
        Ok(messages)
    }
}
