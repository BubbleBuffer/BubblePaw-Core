use super::*;
#[path = "store_prompt_log.rs"]
pub(crate) mod prompt_log;

impl Store {
    pub(super) fn migrate_request_usage(connection: &mut Connection, version: i64) -> Result<()> {
        if version < 8 {
            let migration = connection.transaction()?;
            migration.execute_batch(
                "CREATE TABLE request_usage(
                request_id TEXT PRIMARY KEY,
                created_at TEXT NOT NULL,
                session_id TEXT NOT NULL,
                run_id TEXT,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                kind TEXT NOT NULL CHECK(kind IN ('main','summary_fork','decider')),
                input_tokens INTEGER NOT NULL CHECK(input_tokens>=0),
                cache_read_tokens INTEGER NOT NULL CHECK(cache_read_tokens>=0),
                cache_creation_tokens INTEGER NOT NULL CHECK(cache_creation_tokens>=0),
                output_tokens INTEGER NOT NULL CHECK(output_tokens>=0),
                reasoning_tokens INTEGER CHECK(reasoning_tokens>=0),
                failed INTEGER NOT NULL CHECK(failed IN (0,1))
            );
            CREATE INDEX request_usage_session_time ON request_usage(session_id,created_at);
            CREATE INDEX request_usage_time ON request_usage(created_at);
            PRAGMA user_version=8;",
            )?;
            migration.commit()?;
        }
        if version < 9 {
            let migration = connection.transaction()?;
            migration.execute_batch(
                "ALTER TABLE request_usage ADD COLUMN account TEXT NOT NULL DEFAULT '';
                 UPDATE request_usage SET account=provider;
                 DROP INDEX IF EXISTS one_running;
                 CREATE UNIQUE INDEX one_running ON runs(session_id)
                    WHERE status IN ('running','waiting_for_quota');
                 PRAGMA user_version=9;",
            )?;
            migration.commit()?;
        }
        Ok(())
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
    ) -> Result<()> {
        if let Some(store) = self.scoped(&message.session_id)? {
            return store.append_with_request_usage(message, records);
        }
        self.write(|tx| {
            Self::insert_message(tx, message)?;
            Self::insert_request_usage_tx(tx, records)
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
        let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='request_usage')", [], |row| row.get(0))?;
        if !exists {
            // Read-only queries of pre-migration archives have no per-request
            // facts. Do not fabricate historical rows from run aggregates.
            return Ok(Vec::new());
        }
        // Archives are read-only and may still have schema 8. Account identity
        // was then the provider route; projecting it avoids restoring the chat.
        let has_account = conn
            .prepare("PRAGMA table_info(request_usage)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|name| name == "account");
        let account = if has_account { "account" } else { "provider" };
        let mut statement = conn.prepare(&format!("SELECT request_id,created_at,session_id,run_id,provider,model,kind,input_tokens,cache_read_tokens,cache_creation_tokens,output_tokens,reasoning_tokens,failed,{account} FROM request_usage ORDER BY created_at,request_id"))?;
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
