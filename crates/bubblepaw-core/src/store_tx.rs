//! Runtime batch helpers that take the caller's transaction.
use super::*;

// Runtime batch helpers. They deliberately take the caller's transaction:
// sequence assignment and state changes must commit together under Log's lock.
impl Store {
    pub(crate) fn start_run_tx(tx: &Connection, run: &Run) -> Result<()> {
        tx.execute(
            "INSERT INTO runs(id,session_id,status,data) VALUES(?1,?2,'running',?3)",
            params![run.id, run.session_id, serde_json::to_string(run)?],
        )?;
        Ok(())
    }
    pub(crate) fn admit_tx(
        tx: &Connection,
        run: &Run,
        text: Option<&str>,
        origin: MessageOrigin,
    ) -> Result<()> {
        if Self::has_quota_wait_tx(tx, &run.session_id)? {
            // The host explicitly starts a fresh run from retained history.
            // Clearing in admission prevents a crash from scheduling it twice.
            Self::ensure_quota_effects_resolved(tx, &run.session_id)?;
            Self::clear_quota_wait_tx(tx, &run.session_id, None)?;
        }
        Self::start_run_tx(tx, run)?;
        if let Some(text) = text {
            let input = QueuedInput {
                id: id(),
                session_id: run.session_id.clone(),
                kind: InputKind::Followup,
                text: text.into(),
                state: "pending".into(),
                origin: Some(origin),
            };
            Self::queue_tx(tx, &input)?;
        }
        Self::consume_tx(tx, &run.session_id, false, true)?;
        Ok(())
    }
    pub(crate) fn queue_tx(tx: &Connection, input: &QueuedInput) -> Result<()> {
        let (hash, format) = Self::input_content_ref(tx, &input.text)?;
        tx.execute("INSERT INTO inputs(id,session_id,kind,text,state,origin,content_blob,content_format) VALUES(?1,?2,?3,'','pending',?4,?5,?6)",
            params![input.id, input.session_id, if input.kind == InputKind::Steer {"steer"} else {"followup"},
                input.origin.map(|origin| serde_json::to_string(&origin)).transpose()?, hash, format])?;
        tx.execute(
            "UPDATE sessions SET data=json_set(data,'$.updated_at',?2) WHERE id=?1",
            params![input.session_id, now()],
        )?;
        Ok(())
    }
    pub(crate) fn finish_run_tx(
        tx: &Connection,
        run: &Run,
        result_message_id: Option<&str>,
    ) -> Result<()> {
        if run.status != "interrupted" {
            Self::clear_quota_wait_tx(tx, &run.session_id, Some(&run.id))?;
        }
        tx.execute(
            "UPDATE runs SET status=?2,data=?3,result_message_id=?4 WHERE id=?1",
            params![
                run.id,
                run.status,
                Self::run_metadata(tx, run)?,
                result_message_id
            ],
        )?;
        Ok(())
    }
    pub(crate) fn dispatch_tx(
        tx: &Connection,
        session_id: &str,
        run_id: &str,
        call: &ToolCall,
    ) -> Result<String> {
        let invocation = id();
        tx.execute("INSERT INTO effects(id,session_id,run_id,call_id,state) VALUES(?1,?2,?3,?4,'dispatched')",
            params![invocation,session_id,run_id,call.id])?;
        Ok(invocation)
    }
    pub(crate) fn tool_completed_tx(
        tx: &Connection,
        message: &Message,
        invocation: Option<&str>,
        pending: bool,
    ) -> Result<()> {
        Self::insert_message(tx, message)?;
        if let Some(invocation) = invocation {
            tx.execute(
                "UPDATE effects SET state=CASE WHEN ?2 THEN state ELSE 'completed' END WHERE id=?1",
                params![invocation, pending],
            )?;
        }
        Ok(())
    }
    pub(crate) fn append_output_tx(tx: &Connection, record: &OutputRecord) -> Result<()> {
        let data = serde_json::to_string(record)?;
        anyhow::ensure!(data.len() <= 64 * 1024, "Output exceeds size limit");
        let (count,bytes): (i64,i64) = tx.query_row("SELECT COUNT(*),COALESCE(SUM(length(CAST(data AS BLOB))),0) FROM outputs WHERE execution_id=?1", [&record.execution_id], |row| Ok((row.get(0)?,row.get(1)?)))?;
        anyhow::ensure!(
            count < 128 && bytes + data.len() as i64 <= 256 * 1024,
            "Execution output exceeds size limit"
        );
        tx.execute(
            "INSERT INTO outputs(id,session_id,execution_id,data) VALUES(?1,?2,?3,?4)",
            params![record.id, record.session_id, record.execution_id, data],
        )?;
        Ok(())
    }
}
