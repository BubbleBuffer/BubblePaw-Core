//! Run records, message insertion and effect boundaries.
use super::*;

impl Store {
    pub(crate) fn run_from_data(conn: &Connection, data: &str) -> Result<Run> {
        let mut value: serde_json::Value = serde_json::from_str(data)?;
        if let Some(hash) = value.get("_error_blob").and_then(|hash| hash.as_str()) {
            value["error"] =
                serde_json::Value::String(String::from_utf8(Self::blob_get(conn, hash)?)?);
        }
        Ok(serde_json::from_value(value)?)
    }

    pub(super) fn run_metadata(conn: &Connection, run: &Run) -> Result<String> {
        let mut value = serde_json::to_value(run)?;
        value["text"] = serde_json::Value::Null;
        if let Some(error) = &run.error {
            let hash = Self::blob_put(conn, error.as_bytes())?;
            value["error"] = serde_json::Value::Null;
            value["_error_blob"] = serde_json::Value::String(hash);
        }
        Ok(serde_json::to_string(&value)?)
    }

    pub fn run(&self, run_id: &str) -> Result<Run> {
        if self.0.layout.is_some() {
            return self.find_record("runs", run_id)?.run(run_id);
        }
        let (value, _) = self.run_row(run_id)?;
        self.read(|conn| Self::run_from_data(conn, &value))
    }
    pub(crate) fn hydrate_run(&self, mut run: Run) -> Result<Run> {
        if let Some(store) = self.scoped(&run.session_id)? {
            return store.hydrate_run(run);
        }
        if run.text.is_some() {
            return Ok(run);
        }
        {
            let (_, result) = self.run_row(&run.id)?;
            let conn = self.0.connection.lock().unwrap();
            if let Some(message_id) = result {
                run.text = Some(Self::read_message(&conn, &run.session_id, &message_id)?.content);
                return Ok(run);
            }
        }
        // Results belong to completed messages, not the idle runtime's metadata cache.
        if run.status == "completed" {
            let start = chrono::DateTime::parse_from_rfc3339(&run.started_at)?;
            let end = chrono::DateTime::parse_from_rfc3339(
                run.ended_at
                    .as_deref()
                    .context("Completed run has no end time")?,
            )?;
            let conn = self.0.connection.lock().unwrap();
            let mut statement =
                conn.prepare("SELECT data FROM messages WHERE session_id=?1 ORDER BY seq DESC")?;
            let mut rows = statement.query([&run.session_id])?;
            while let Some(row) = rows.next()? {
                let message = Self::message_from_data(&conn, &row.get::<_, String>(0)?)?;
                let created = chrono::DateTime::parse_from_rfc3339(&message.created_at)?;
                if created < start {
                    break;
                }
                // A script run ends on its tool result, not on an assistant message.
                let result = message.role == Role::Assistant
                    || (run.model == crate::SCRIPT_MODEL && message.role == Role::Tool);
                if created <= end && result {
                    run.text = Some(message.content);
                    break;
                }
            }
        }
        Ok(run)
    }
    pub fn runs(&self, session_id: &str) -> Result<Vec<Run>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.all_runs(session_id);
        }
        self.all_runs(session_id)
    }
    #[cfg(test)]
    pub(crate) fn start_run(&self, run: &Run) -> Result<()> {
        if let Some(store) = self.scoped(&run.session_id)? {
            return store.start_run(run);
        }
        self.write(|tx| {
            tx.execute(
                "INSERT INTO runs(id,session_id,status,data) VALUES(?1,?2,'running',?3)",
                params![run.id, run.session_id, serde_json::to_string(run)?],
            )?;
            Ok(())
        })
    }
    pub(crate) fn finish_run(&self, run: &Run) -> Result<()> {
        if let Some(store) = self.scoped(&run.session_id)? {
            return store.finish_run(run);
        }
        self.write(|tx| {
            tx.execute(
                "UPDATE runs SET status=?2,data=?3 WHERE id=?1",
                params![run.id, run.status, Self::run_metadata(tx, run)?],
            )?;
            Ok(())
        })
    }
    pub(crate) fn message(session_id: &str, role: Role, content: String) -> Message {
        Message {
            id: id(),
            session_id: session_id.into(),
            role,
            origin: None,
            content,
            tool_calls: Vec::new(),
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: now(),
        }
    }
    pub(crate) fn insert_message(tx: &Connection, message: &Message) -> Result<()> {
        Self::insert_message_with_blobs(tx, message, false)
    }

    pub(super) fn insert_message_with_blobs(
        tx: &Connection,
        message: &Message,
        blob_large_all_roles: bool,
    ) -> Result<()> {
        let mut data = serde_json::to_value(message)?;
        if let Some(state) = &message.provider_state {
            let opaque = serde_json::to_vec(state)?;
            if opaque.len() > 4096 {
                let hash = Self::blob_put(tx, &opaque)?;
                data.as_object_mut().unwrap().remove("provider_state");
                data["_provider_state_blob"] = serde_json::Value::String(hash);
            }
        }
        // Failed script runs expose this same body as Run.error. Blob-back
        // even a small error so the terminal record can share its exact hash.
        if let Some(parts) = Self::attachment_content_parts(tx, &message.content)? {
            data["content"] = serde_json::Value::String(String::new());
            let hash = Self::blob_put(tx, &serde_json::to_vec(&parts)?)?;
            data["_content_parts_blob"] = serde_json::Value::String(hash);
        } else if (message.role == Role::Tool && message.is_error)
            || (message.content.len() > 4096
                && (blob_large_all_roles
                    || message.role == Role::Tool
                    || message.content.contains("<BubblePawAttachments>")))
        {
            let hash = Self::blob_put(tx, message.content.as_bytes())?;
            data["content"] = serde_json::Value::String(String::new());
            data["_content_blob"] = serde_json::Value::String(hash);
        } else if message.role == Role::User {
            let hash = crate::fingerprint(message.content.as_bytes());
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM blobs WHERE hash=?1)",
                [&hash],
                |row| row.get(0),
            )?;
            if exists {
                data["content"] = serde_json::Value::String(String::new());
                data["_content_blob"] = serde_json::Value::String(hash);
            }
        }
        tx.execute(
            "INSERT INTO messages(session_id,message_id,data) VALUES(?1,?2,?3)",
            params![
                message.session_id,
                message.id,
                serde_json::to_string(&data)?
            ],
        )?;
        let seq = tx.last_insert_rowid();
        Self::insert_message_header(tx, seq, message)?;
        for call in &message.tool_calls {
            tx.execute(
                "INSERT OR IGNORE INTO message_tool_calls VALUES(?1,?2,?3,'call')",
                params![message.session_id, call.id, seq],
            )?;
        }
        if message.role == Role::Tool
            && let Some(call) = &message.tool_call_id
        {
            tx.execute(
                "INSERT OR IGNORE INTO message_tool_calls VALUES(?1,?2,?3,'result')",
                params![message.session_id, call, seq],
            )?;
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn dispatch(
        &self,
        session_id: &str,
        run_id: &str,
        call: &ToolCall,
    ) -> Result<String> {
        if let Some(store) = self.scoped(session_id)? {
            return store.dispatch(session_id, run_id, call);
        }
        let invocation = id();
        self.write(|tx| {
            tx.execute(
                "INSERT INTO effects(id,session_id,run_id,call_id,state) VALUES(?1,?2,?3,?4,'dispatched')",
                params![invocation, session_id, run_id, call.id],
            )?;
            Ok(())
        })?;
        Ok(invocation)
    }
    pub(crate) fn dispatch_nested(
        &self,
        session: &str,
        run: &str,
        call: &ToolCall,
    ) -> Result<String> {
        if let Some(store) = self.scoped(session)? {
            return store.dispatch_nested(session, run, call);
        }
        let invocation = id();
        self.write(|tx| {
            let hash = Self::blob_put(tx, &serde_json::to_vec(&call.arguments)?)?;
            let detail = serde_json::json!({"id":call.id,"name":call.name,"_arguments_blob":hash});
            tx.execute("INSERT INTO effects(id,session_id,run_id,call_id,state,detail) VALUES(?1,?2,?3,?4,'dispatched',?5)", params![invocation,session,run,call.id,serde_json::to_string(&detail)?])?;
            Ok(())
        })?;
        Ok(invocation)
    }
    pub(crate) fn effect_completed(&self, invocation: &str) -> Result<()> {
        if self.0.layout.is_some() {
            return self
                .find_record("effects", invocation)?
                .effect_completed(invocation);
        }
        self.write(|tx| {
            tx.execute(
                "UPDATE effects SET state='completed' WHERE id=?1",
                [invocation],
            )?;
            Ok(())
        })
    }
    pub(crate) fn effect_unknown(&self, invocation: &str) -> Result<()> {
        if self.0.layout.is_some() {
            return self
                .find_record("effects", invocation)?
                .effect_unknown(invocation);
        }
        self.write(|tx| {
            tx.execute(
                "UPDATE effects SET state='unknown' WHERE id=?1 AND state='dispatched'",
                [invocation],
            )?;
            Ok(())
        })
    }
    pub fn outputs(&self, session: &str, limit: usize) -> Result<Vec<OutputRecord>> {
        if let Some(store) = self.scoped(session)? {
            return store.outputs(session, limit);
        }
        let conn = self.0.connection.lock().unwrap();
        let mut statement=conn.prepare("SELECT data FROM (SELECT seq,data FROM outputs WHERE session_id=?1 ORDER BY seq DESC LIMIT ?2) ORDER BY seq")?;
        statement
            .query_map(params![session, limit.min(512) as i64], |r| {
                r.get::<_, String>(0)
            })?
            .map(|r| Ok(serde_json::from_str(&r?)?))
            .collect()
    }
    /// Effect diagnostics are explicit, bounded and kept out of normal model history.
    pub fn pending_effects(&self, session: &str) -> Result<Vec<serde_json::Value>> {
        if let Some(store) = self.scoped(session)? {
            return store.pending_effects(session);
        }
        let conn = self.0.connection.lock().unwrap();
        let mut statement=conn.prepare("SELECT id,run_id,state,detail FROM effects WHERE session_id=?1 AND state!='completed' LIMIT 64")?;
        statement
            .query_map([session], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })?
            .map(|r| {
                let (id, run, state, detail) = r?;
                let call = detail
                    .and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok())
                    .map(|mut value| -> Result<_> {
                        if let Some(hash) =
                            value.get("_arguments_blob").and_then(|hash| hash.as_str())
                        {
                            value["arguments"] =
                                serde_json::from_slice(&Self::blob_get(&conn, hash)?)?;
                            value.as_object_mut().unwrap().remove("_arguments_blob");
                        }
                        Ok(value)
                    })
                    .transpose()?;
                Ok(serde_json::json!({"id":id,"run_id":run,"state":state,"call":call}))
            })
            .collect()
    }
    pub(crate) fn close_unresolved(&self, session_id: &str, run_id: &str) -> Result<()> {
        if let Some(store) = self.scoped(session_id)? {
            return store.close_unresolved(session_id, run_id);
        }
        let messages = self.messages(session_id)?;
        let mut pending = Vec::new();
        for msg in messages {
            if msg.role == Role::Assistant {
                pending.extend(msg.tool_calls);
            } else if let Some(call_id) = msg.tool_call_id {
                pending.retain(|c| c.id != call_id);
            }
        }
        if pending.is_empty() {
            return Ok(());
        }
        self.write(|tx|{for call in pending {let dispatched:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM effects WHERE run_id=?1 AND call_id=?2)",params![run_id,call.id],|r|r.get(0))?;let result=ToolResult::error(if dispatched {"Tool outcome unknown after interruption; effect may have occurred. Do not replay automatically."} else {"Tool was not dispatched because the run was interrupted."});let mut msg=Self::message(session_id,Role::Tool,result.content);msg.tool_call_id=Some(call.id);msg.is_error=true;Self::insert_message(tx,&msg)?;}tx.execute("UPDATE effects SET state='unknown' WHERE run_id=?1 AND state='dispatched'",[run_id])?;Ok(())})
    }
}
