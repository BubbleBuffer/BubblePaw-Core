use super::*;
#[path = "store_prompt_log.rs"]
pub(crate) mod prompt_log;

impl Store {
    /// Persist physical billing rows and link only the explicitly retained response contributors.
    pub(crate) fn insert_message_request_usage_tx(
        tx: &Connection,
        message: &Message,
        records: &[RequestUsage],
        request_ids: &[String],
    ) -> Result<()> {
        Self::insert_request_usage_tx(tx, records)?;
        if request_ids.is_empty() {
            return Ok(());
        }
        anyhow::ensure!(
            message.role == Role::Assistant,
            "Only assistant replies own provider output"
        );
        let seq = Self::session_message_seq(tx, &message.session_id, &message.id)?;
        for request_id in request_ids {
            let record = records
                .iter()
                .find(|record| record.request_id == *request_id)
                .context("Message output references an absent request")?;
            anyhow::ensure!(
                !record.failed
                    && record.kind == crate::RequestKind::Main
                    && record.session_id == message.session_id,
                "Message output must reference a successful main request in the same session"
            );
            tx.execute(
                "UPDATE request_usage SET message_seq=?2 WHERE request_id=?1 AND message_seq IS NULL AND failed=0 AND kind='main' AND session_id=?3",
                params![request_id, seq, message.session_id],
            )?;
        }
        Ok(())
    }

    /// Read provider output reports in canonical order using indexed metadata only.
    /// Missing or incomplete reports stay None; estimation belongs to the host.
    pub fn reported_message_tokens(
        &self,
        session_id: &str,
        range: std::ops::RangeInclusive<i64>,
    ) -> Result<Vec<crate::ReportedMessageTokens>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.reported_message_tokens(session_id, range);
        }
        self.read(|conn| {
            let mut statement = conn.prepare(
                "SELECT m.message_id,(CASE WHEN h.role='assistant' THEN
                    (SELECT SUM(u.output_tokens) FROM request_usage u
                    WHERE u.session_id=m.session_id AND u.message_seq=m.seq
                        AND u.failed=0 AND u.kind='main') END)
                 FROM messages m JOIN message_headers h ON h.message_seq=m.seq
                 WHERE m.session_id=?1 AND m.seq BETWEEN ?2 AND ?3 ORDER BY m.seq",
            )?;
            statement
                .query_map(params![session_id, range.start(), range.end()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
                })?
                .map(|row| {
                    let (message_id, tokens) = row?;
                    Ok(crate::ReportedMessageTokens {
                        message_id,
                        tokens: tokens.map(u64::try_from).transpose()?,
                    })
                })
                .collect()
        })
    }

    pub(crate) fn insert_request_usage_tx(tx: &Connection, records: &[RequestUsage]) -> Result<()> {
        let token_count = |value: u64| {
            i64::try_from(value).context("Request token count exceeds SQLite INTEGER range")
        };
        for record in records {
            // Never rewrite historical billing metadata on retry. Replaying an
            // identical completion is harmless; UUIDs identify physical attempts.
            tx.execute("INSERT INTO request_usage(request_id,created_at,session_id,run_id,provider,model,kind,input_tokens,cache_read_tokens,cache_creation_tokens,output_tokens,reasoning_tokens,failed,account) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14) ON CONFLICT(request_id) DO NOTHING", params![
                record.request_id, record.created_at, record.session_id, record.run_id,
                record.provider, record.model, serde_json::to_value(record.kind)?.as_str().unwrap(),
                token_count(record.usage.input_tokens)?, token_count(record.usage.cache_read_tokens)?,
                token_count(record.usage.cache_creation_tokens)?, token_count(record.usage.output_tokens)?,
                record.usage.reasoning_tokens.map(token_count).transpose()?, record.failed,
                record.account,
            ])?;
        }
        Ok(())
    }

    /// Stage an auxiliary completion immediately before its canonical state
    /// change. The next chat transaction commits both, without another COMMIT.
    /// If no such state change follows, call `flush_request_usage` before return.
    /// Records survive transaction rollback and are deduplicated by request UUID.
    pub fn queue_request_usage(&self, record: RequestUsage) -> Result<()> {
        if let Some(store) = self.scoped(&record.session_id)? {
            return store.queue_request_usage(record);
        }
        self.session(&record.session_id)?;
        self.0.pending_usage.lock().unwrap().push(record);
        Ok(())
    }

    /// Flush staged completions for one chat only. Empty flushes do not write.
    pub fn flush_request_usage(&self, session_id: &str) -> Result<()> {
        if let Some(store) = self.scoped(session_id)? {
            return store.flush_request_usage(session_id);
        }
        if self.0.pending_usage.lock().unwrap().is_empty() {
            return Ok(());
        }
        self.write(|_| Ok(()))
    }

    pub(crate) fn append_with_request_usage(
        &self,
        message: &Message,
        records: &[RequestUsage],
        request_ids: &[String],
    ) -> Result<()> {
        if let Some(store) = self.scoped(&message.session_id)? {
            return store.append_with_request_usage(message, records, request_ids);
        }
        self.write(|tx| {
            Self::insert_message(tx, message)?;
            Self::insert_message_request_usage_tx(tx, message, records, request_ids)
        })
    }

    /// Read authoritative records, including archived chats without restoring
    /// them. This does not flush pending records or run maintenance.
    pub fn request_usage(&self) -> Result<Vec<RequestUsage>> {
        if let Some(layout) = &self.0.layout {
            return Self::read_request_usage_data_dir(&layout.data_dir);
        }
        self.read(Self::request_usage_from_connection)
    }

    pub(super) fn request_usage_from_connection(conn: &Connection) -> Result<Vec<RequestUsage>> {
        let mut statement = conn.prepare("SELECT request_id,created_at,session_id,run_id,provider,model,kind,input_tokens,cache_read_tokens,cache_creation_tokens,output_tokens,reasoning_tokens,failed,account FROM request_usage ORDER BY created_at,request_id")?;
        let mut rows = statement.query([])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            let kind: String = row.get(6)?;
            records.push(RequestUsage {
                request_id: row.get(0)?,
                created_at: row.get(1)?,
                session_id: row.get(2)?,
                run_id: row.get(3)?,
                provider: row.get(4)?,
                account: row.get(13)?,
                model: row.get(5)?,
                kind: serde_json::from_value(serde_json::Value::String(kind))?,
                usage: Usage {
                    input_tokens: u64::try_from(row.get::<_, i64>(7)?)?,
                    cache_read_tokens: u64::try_from(row.get::<_, i64>(8)?)?,
                    cache_creation_tokens: u64::try_from(row.get::<_, i64>(9)?)?,
                    output_tokens: u64::try_from(row.get::<_, i64>(10)?)?,
                    reasoning_tokens: row
                        .get::<_, Option<i64>>(11)?
                        .map(u64::try_from)
                        .transpose()?,
                },
                failed: row.get(12)?,
            });
        }
        Ok(records)
    }
}
