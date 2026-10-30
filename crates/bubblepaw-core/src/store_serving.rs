//! Disposable projections buffer in SQLite's page cache until an explicit flush.
use super::*;
const EVENT_RESERVATION: u64 = 1 << 32;
// The Session API's browser clients receive JSON numbers, so sequence IDs must
// remain exactly representable in both JavaScript and SQLite.
const MAX_EVENT_SEQUENCE: u64 = (1 << 53) - 1;
const RESERVED_KEY: &str = "events_reserved_end";
pub(super) struct ProjectionRows {
    table: &'static str,
    columns: Vec<String>,
    rows: Vec<Vec<rusqlite::types::Value>>,
}

impl Store {
    pub(crate) fn runtime_idle(&self) -> Result<()> {
        if self.is_layout() {
            self.layout_runtime_idle()
        } else {
            self.idle_checkpoint()
        }
    }

    pub(super) fn initialize_event_sequences(&self) -> Result<()> {
        let layout = self
            .0
            .layout
            .as_ref()
            .context("Sequence reservation requires a directory store")?;
        let catalog_reserved: Option<u64> = layout
            .catalog
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [RESERVED_KEY],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|value| value.parse())
            .transpose()?;
        let (last, pruned, serving_reserved) = self.read(|conn| {
            let (last, pruned) = crate::event_log::bounds(conn)?;
            let reserved: Option<String> = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [RESERVED_KEY],
                    |row| row.get(0),
                )
                .optional()?;
            Ok((
                last,
                pruned,
                reserved.map(|value| value.parse::<u64>()).transpose()?,
            ))
        })?;
        let old_reserved = catalog_reserved.into_iter().chain(serving_reserved).max();
        let recovery_floor = if let Some(reserved) = old_reserved {
            reserved.max(last)
        } else if last == 0 && !self.list_sessions()?.is_empty() {
            // If both disposable receipts were removed, move into a fresh
            // time-based epoch rather than restarting old clients at sequence 1.
            (chrono::Utc::now().timestamp_millis().max(0) as u64)
                .checked_mul(1 << 10)
                .context("Event sequence epoch overflow")?
        } else {
            last
        };
        self.reserve_event_sequences(recovery_floor)?;
        self.runtime().log.seed(
            recovery_floor,
            if recovery_floor > last {
                pruned.max(recovery_floor)
            } else {
                pruned
            },
        );
        Ok(())
    }

    fn reserve_event_sequences(&self, floor: u64) -> Result<()> {
        let layout = self
            .0
            .layout
            .as_ref()
            .context("Sequence reservation requires a directory store")?;
        self.flush_serving()?;
        let end = floor
            .checked_add(EVENT_RESERVATION)
            .filter(|end| *end <= MAX_EVENT_SEQUENCE)
            .context("Event sequence reservation exhausted")?;
        {
            let mut catalog = layout.catalog.lock().unwrap();
            let tx = catalog.transaction()?;
            tx.execute("INSERT INTO metadata VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![RESERVED_KEY,end.to_string()])?;
            tx.commit()?;
            layout.catalog_transactions.fetch_add(1, Ordering::Relaxed);
        }
        layout.event_reserved_end.store(end, Ordering::Relaxed);
        self.write(|conn| {
            conn.execute("INSERT INTO metadata VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![RESERVED_KEY,end.to_string()])?;
            Ok(())
        })?;
        Ok(())
    }

    pub(crate) fn ensure_event_sequences(&self, last: u64) -> Result<()> {
        if let Some(host) = self.host() {
            return host.ensure_event_sequences(last);
        }
        let Some(layout) = &self.0.layout else {
            return Ok(());
        };
        if last > layout.event_reserved_end.load(Ordering::Relaxed) {
            self.reserve_event_sequences(last)?;
        }
        Ok(())
    }

    /// Resolved operational data can leave the record in its owning durable
    /// step. Stage only metadata; accepted text already belongs to messages.
    pub(super) fn split_operations(&self, conn: &Connection) -> Result<Vec<ProjectionRows>> {
        if self.host().is_none() {
            return Ok(Vec::new());
        }
        let mut batches = Vec::new();
        for (table, predicate) in [
            (
                "runs",
                "status NOT IN ('running','interrupted','waiting_for_quota')",
            ),
            ("approvals", "state!='pending'"),
            ("inputs", "state NOT IN ('pending','reminder_pending')"),
        ] {
            let mut statement =
                conn.prepare_cached(&format!("SELECT * FROM {table} WHERE {predicate}"))?;
            let mut columns = statement
                .column_names()
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let count = statement.column_count();
            let mut rows = statement
                .query_map([], |row| {
                    (0..count)
                        .map(|index| row.get(index))
                        .collect::<rusqlite::Result<Vec<rusqlite::types::Value>>>()
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if rows.is_empty() {
                continue;
            }
            if table == "inputs" {
                for column in &mut columns {
                    if column == "seq" {
                        *column = "source_seq".into();
                    }
                }
                columns.push("resolved_at".into());
                for row in &mut rows {
                    row.push(now().into());
                }
            }
            batches.push(ProjectionRows {
                table,
                columns,
                rows,
            });
            conn.execute(&format!("DELETE FROM {table} WHERE {predicate}"), [])?;
        }
        conn.execute(
            "UPDATE effects SET detail=NULL WHERE state='completed' AND detail IS NOT NULL",
            [],
        )?;
        Ok(batches)
    }

    pub(super) fn save_operations_best_effort(&self, batches: &[ProjectionRows]) {
        if batches.is_empty() {
            return;
        }
        if let Some(host) = self.host() {
            let saved = host.write(|conn| {
                for batch in batches {
                    let values = std::iter::repeat_n("?", batch.columns.len())
                        .collect::<Vec<_>>()
                        .join(",");
                    let sql = format!(
                        "INSERT OR REPLACE INTO {}({}) VALUES({values})",
                        batch.table,
                        batch.columns.join(",")
                    );
                    let mut statement = conn.prepare(&sql)?;
                    for row in &batch.rows {
                        statement.execute(rusqlite::params_from_iter(row))?;
                    }
                }
                Ok(())
            });
            if saved.is_err()
                && let Some(layout) = &host.0.layout
            {
                layout.projection_gap.store(true, Ordering::Relaxed);
            }
        }
    }

    pub(crate) fn take_projection_gap(&self) -> bool {
        if let Some(host) = self.host() {
            return host.take_projection_gap();
        }
        self.0
            .layout
            .as_ref()
            .is_some_and(|layout| layout.projection_gap.swap(false, Ordering::Relaxed))
    }
    pub(super) fn insert_message_header(
        conn: &Connection,
        seq: i64,
        message: &Message,
    ) -> Result<()> {
        let role = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        conn.execute(
            "INSERT INTO message_headers VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                seq,
                message.session_id,
                role,
                message
                    .origin
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                crate::is_typed_user(message),
                message.created_at,
                message.is_error
            ],
        )?;
        Ok(())
    }

    /// Stream identity, role, typed-origin, time and call/result references.
    /// Returned messages have empty bodies, arguments, names and provider state.
    /// No canonical body or blob is decoded. The callback must not reenter Store.
    pub fn for_each_message_header(
        &self,
        session: &str,
        mut visit: impl FnMut(i64, Message) -> Result<()>,
    ) -> Result<usize> {
        if let Some(chat) = self.scoped(session)? {
            return chat.for_each_message_header(session, visit);
        }
        self.read(|conn| {
            let mut statement = conn.prepare("SELECT h.message_seq,m.message_id,h.role,h.origin,h.typed_user,h.created_at,h.is_error FROM message_headers h JOIN messages m ON m.seq=h.message_seq WHERE h.session_id=?1 ORDER BY h.message_seq")?;
            let mut calls = conn.prepare_cached("SELECT call_id,kind FROM message_tool_calls WHERE session_id=?1 AND message_seq=?2 ORDER BY kind,call_id")?;
            let mut rows = statement.query([session])?;
            let mut count = 0;
            while let Some(row) = rows.next()? {
                let seq: i64 = row.get(0)?;
                let role = match row.get::<_,String>(2)?.as_str() { "system" => Role::System, "user" => Role::User, "assistant" => Role::Assistant, "tool" => Role::Tool, _ => anyhow::bail!("Unknown canonical message header role") };
                let origin: Option<String> = row.get(3)?;
                let typed_user: bool = row.get(4)?;
                let origin = origin.map(|origin| serde_json::from_str(&origin)).transpose()?.or_else(|| if role==Role::User { Some(if typed_user { crate::MessageOrigin::Typed } else { crate::MessageOrigin::Generated }) } else { None });
                let mut message = Message { id: row.get(1)?, session_id: session.into(), role, origin, content: String::new(), tool_calls: Vec::new(), tool_call_id: None, provider_state: None, is_error: row.get(6)?, created_at: row.get(5)? };
                for call in calls.query_map(params![session,seq], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))? {
                    let (id,kind) = call?;
                    if kind == "call" { message.tool_calls.push(ToolCall {id,name:String::new(),arguments:serde_json::Value::Null}); } else { message.tool_call_id=Some(id); }
                }
                visit(seq,message)?;
                count += 1;
            }
            Ok(count)
        })
    }

    pub(crate) fn has_tool_call(&self, session: &str, call: &str) -> Result<bool> {
        if let Some(chat) = self.scoped(session)? {
            return chat.has_tool_call(session, call);
        }
        self.read(|conn| Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM message_tool_calls WHERE session_id=?1 AND call_id=?2 AND kind='call')",params![session,call],|row|row.get(0))?))
    }

    /// Pending provider call IDs at an exact canonical boundary. These are
    /// indexed references, so validating a new summary never decodes cold bodies.
    pub fn pending_call_ids_through(&self, session: &str, frontier: i64) -> Result<Vec<String>> {
        if let Some(chat) = self.scoped(session)? {
            return chat.pending_call_ids_through(session, frontier);
        }
        self.read(|conn| Self::pending_call_ids_at(conn, session, frontier))
    }

    pub(super) fn pending_call_ids_at(
        conn: &Connection,
        session: &str,
        frontier: i64,
    ) -> Result<Vec<String>> {
        let mut statement = conn.prepare("SELECT call_id FROM message_tool_calls WHERE session_id=?1 AND message_seq<=?2 GROUP BY call_id
            HAVING MAX(CASE WHEN kind='call' THEN message_seq END)>COALESCE(MAX(CASE WHEN kind='result' THEN message_seq END),0) ORDER BY call_id")?;
        Ok(statement
            .query_map(params![session, frontier], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Read an imported source's latest usable compaction and its canonical
    /// boundary without decoding covered history. The summary remains derived
    /// source material in the chat; this small model projection is disposable.
    pub fn imported_history_seed(&self, session: &str) -> Result<(Option<i64>, Vec<Message>)> {
        if let Some(chat) = self.scoped(session)? {
            return chat.imported_history_seed(session);
        }
        self.read(|conn| Self::imported_history_seed_at(conn, session))
    }

    pub(super) fn imported_history_seed_at(
        conn: &Connection,
        session: &str,
    ) -> Result<(Option<i64>, Vec<Message>)> {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='imported_derived')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok((None, Vec::new()));
        }
        let mut statement = conn.prepare("SELECT boundary_seq,data FROM imported_derived WHERE session_id=?1 AND kind IN ('codex_compaction','claude_compaction_summary') AND boundary_seq IS NOT NULL ORDER BY source_line DESC,id DESC")?;
        let mut rows = statement.query([session])?;
        while let Some(row) = rows.next()? {
            let boundary: i64 = row.get(0)?;
            let record: crate::transcript::ImportDerivedRecord =
                serde_json::from_str(&row.get::<_, String>(1)?)?;
            let Some(text) = Self::import_summary_text(&record) else {
                continue;
            };
            return Ok((
                Some(boundary),
                vec![Message {
                    id: format!("import-summary:{}", record.id),
                    session_id: session.into(),
                    role: Role::System,
                    origin: None,
                    content: text,
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    provider_state: None,
                    is_error: false,
                    created_at: record.created_at,
                }],
            ));
        }
        Ok((None, Vec::new()))
    }

    pub(super) fn import_summary_text(
        record: &crate::transcript::ImportDerivedRecord,
    ) -> Option<String> {
        let value = record
            .content
            .get("message")
            .or_else(|| record.content.get("content"))
            .unwrap_or(&record.content);
        let text = if let Some(text) = value.as_str() {
            text.to_owned()
        } else {
            value
                .as_array()?
                .iter()
                .filter_map(|part| part.get("text").and_then(|text| text.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        };
        (!text.trim().is_empty()).then_some(text)
    }

    /// Model-serving history for a source import: latest imported summary plus
    /// raw tail. For ordinary sessions this retains the full-history default.
    /// Host-specific frozen views and masking remain ContextPreparation's job.
    pub fn serving_messages(&self, session: &str) -> Result<Vec<Message>> {
        let (frontier, mut prefix) = self.imported_history_seed(session)?;
        prefix.extend(
            self.messages_after(session, frontier.unwrap_or(0))?
                .into_iter()
                .map(|(_, message)| message),
        );
        Ok(prefix)
    }

    pub(super) fn write_serving<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let started = std::time::Instant::now();
        let result = (|| {
            let conn = self.0.connection.lock().unwrap();
            let started_buffer = conn.is_autocommit();
            if started_buffer {
                conn.execute_batch("BEGIN DEFERRED")?;
            }
            conn.execute_batch("SAVEPOINT serving_operation")?;
            match f(&conn) {
                Ok(value) => {
                    conn.execute_batch("RELEASE serving_operation")?;
                    Ok(value)
                }
                Err(error) => {
                    conn.execute_batch("ROLLBACK TO serving_operation; RELEASE serving_operation")?;
                    if started_buffer {
                        conn.execute_batch("ROLLBACK")?;
                    }
                    Err(error)
                }
            }
        })();
        self.0.write_nanoseconds.fetch_add(
            started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        result
    }

    /// Flush disposable journal/search/view writes and the rebuildable catalog.
    /// Record writes already committed synchronously before entering this buffer.
    /// Readers in this host see the buffered projection immediately. Interruption
    /// may lose the pending projection; it never loses or replays a record effect.
    pub fn flush_serving(&self) -> Result<()> {
        if let Some(host) = self.host() {
            return host.flush_serving();
        }
        let Some(layout) = &self.0.layout else {
            return Ok(());
        };
        let mut pending = layout.pending_catalog.lock().unwrap();
        let conn = self.0.connection.lock().unwrap();
        if conn.is_autocommit() && pending.is_empty() {
            return Ok(());
        }
        if conn.is_autocommit() {
            conn.execute_batch("BEGIN DEFERRED")?;
        }
        conn.execute_batch("SAVEPOINT catalog_flush")?;
        let updated = pending.values().try_for_each(|(root, session, activity)| {
            Self::catalog_session_to(
                &conn,
                "catalog_projection.sessions",
                root,
                session,
                activity,
            )
        });
        if let Err(error) = updated {
            conn.execute_batch("ROLLBACK TO catalog_flush; RELEASE catalog_flush")?;
            return Err(error);
        }
        conn.execute_batch("RELEASE catalog_flush; COMMIT")?;
        self.0.transactions.fetch_add(1, Ordering::Relaxed);
        layout.serving_transactions.fetch_add(1, Ordering::Relaxed);
        // This is one application COMMIT with both disposable files attached.
        // Physical WAL writes and crash atomicity are deliberately separate.
        pending.clear();
        Ok(())
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Graceful close preserves buffered operational history; abrupt process
        // loss is explicitly allowed to roll back these disposable projections.
        if let Some(layout) = &self.layout
            && let (Ok(conn), Ok(pending)) = (self.connection.lock(), layout.pending_catalog.lock())
        {
            if conn.is_autocommit() && !pending.is_empty() {
                let _ = conn.execute_batch("BEGIN DEFERRED");
            }
            if !conn.is_autocommit() {
                let saved = pending.values().try_for_each(|(root, session, activity)| {
                    Store::catalog_session_to(
                        &conn,
                        "catalog_projection.sessions",
                        root,
                        session,
                        activity,
                    )
                });
                if saved.is_ok() {
                    let _ = conn.execute_batch("COMMIT");
                }
            }
        }
    }
}
