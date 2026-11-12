//! Session lifecycle: listing, rename, delete and fork.
use super::*;

impl Store {
    pub fn session(&self, session_id: &str) -> Result<Session> {
        if let Some(store) = self.scoped(session_id)? {
            return store.session(session_id);
        }
        let conn = self.0.connection.lock().unwrap();
        let value: String =
            conn.query_row("SELECT data FROM sessions WHERE id=?1", [session_id], |r| {
                r.get(0)
            })?;
        Ok(serde_json::from_str(&value)?)
    }
    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        if self.0.layout.is_some() {
            return self.layout_sessions();
        }
        let conn = self.0.connection.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM sessions ORDER BY rowid DESC")?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .map(|r| Ok(serde_json::from_str(&r?)?))
            .collect()
    }
    /// Ordinary host lists omit internal sessions; lifecycle code uses `list_sessions`.
    pub fn list_visible_sessions(&self) -> Result<Vec<Session>> {
        Ok(self
            .list_sessions()?
            .into_iter()
            .filter(|session| !session.hidden)
            .collect())
    }

    /// Bounded metadata for ordinary host projections, filtering before the limit.
    pub fn visible_session_summaries(&self, limit: usize) -> Result<Vec<SessionRecord>> {
        self.session_summaries_impl(limit, true)
    }
    /// Change a session's title; `updated_at` moves too.
    pub fn rename_session(&self, session_id: &str, title: impl Into<String>) -> Result<Session> {
        if let Some(store) = self.scoped(session_id)? {
            return store.rename_session(session_id, title);
        }
        let title = title.into();
        self.write(|tx| {
            let data: String = tx
                .query_row("SELECT data FROM sessions WHERE id=?1", [session_id], |r| {
                    r.get(0)
                })
                .optional()?
                .with_context(|| format!("Unknown session: {session_id}"))?;
            let mut session: Session = serde_json::from_str(&data)?;
            session.title = title;
            session.updated_at = now();
            tx.execute(
                "UPDATE sessions SET data=?2 WHERE id=?1",
                params![session_id, serde_json::to_string(&session)?],
            )?;
            Ok(session)
        })
    }
    /// Delete a session and everything stored for it, in one transaction:
    /// messages, runs, inputs, effects, outputs, events, approvals, approval
    /// rules, checkpoints and chapters. Refused while it has a running run.
    /// Child sessions that name it as parent are kept.
    pub fn delete_session(&self, session_id: &str) -> Result<()> {
        self.delete_session_inner(session_id, false)
    }
    /// Register idempotent cleanup for host-owned session resources. Reusing a
    /// name replaces its process-local hook; reopening requires registration again.
    pub fn set_session_cleanup(&self, name: impl Into<String>, cleanup: SessionCleanup) {
        self.runtime()
            .session_cleanups
            .lock()
            .unwrap()
            .insert(name.into(), cleanup);
    }
    /// Release one host-owned lifecycle hook without affecting other consumers.
    pub fn remove_session_cleanup(&self, name: &str) {
        self.runtime().session_cleanups.lock().unwrap().remove(name);
    }
    pub(super) fn cleanup_deleted_sessions(&self, sessions: &[String]) -> Result<()> {
        let cleanups: Vec<_> = self
            .runtime()
            .session_cleanups
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        let mut first_error = None;
        for session in sessions {
            for cleanup in &cleanups {
                if let Err(error) = cleanup(session) {
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
    /// Host housekeeping may delete only histories with fully resolved effects.
    /// The check shares the deletion admission guard and canonical transaction.
    pub fn delete_session_preserving_effects(&self, session_id: &str) -> Result<()> {
        self.delete_session_inner(session_id, true)
    }
    fn delete_session_inner(&self, session_id: &str, preserve_effects: bool) -> Result<()> {
        if self.0.layout.is_some() {
            return self.layout_delete(session_id, preserve_effects);
        }
        let mut removed = vec![session_id.to_owned()];
        if preserve_effects {
            let sessions = self.list_sessions()?;
            loop {
                let before = removed.len();
                for session in &sessions {
                    if session
                        .parent_session_id
                        .as_ref()
                        .is_some_and(|parent| removed.contains(parent))
                        && !removed.contains(&session.id)
                    {
                        removed.push(session.id.clone());
                    }
                }
                if removed.len() == before {
                    break;
                }
            }
        }
        let runtime = self.runtime();
        let mut admission = runtime.delete_guard(&removed)?;
        // Read before taking the connection: the log locks its state, then the connection.
        let last_event = self.0.runtime.log.last();
        self.write(|tx| {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
                [session_id],
                |r| r.get(0),
            )?;
            anyhow::ensure!(exists, "Unknown session: {session_id}");
            for session in &removed {
            let running: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM runs WHERE session_id=?1 AND status IN ('running','waiting_for_quota'))",
                [session],
                |r| r.get(0),
            )?;
            anyhow::ensure!(!running, "Cannot delete a session with a running run");
            if preserve_effects {
                let unresolved: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM effects WHERE session_id=?1 AND state!='completed')",
                    [session], |row| row.get(0),
                )?;
                anyhow::ensure!(!unresolved, "Cannot delete a session with unresolved effects");
            }
            }
            // Keep the event sequence monotonic across restarts even when the
            // newest events belong to this session.
            crate::event_log::remember_last(tx, last_event)?;
            for session in &removed {
                self.delete_session_checkpoints(tx, session)?;
                tx.execute("UPDATE request_usage SET message_seq=NULL WHERE session_id=?1 AND message_seq IS NOT NULL", [session])?;
            }
            for table in [
                "reminder_metadata",
                "host_state",
                "approvals",
                "approval_rules",
                "events",
                "attachments",
                "context_views",
                "context_checkpoint_versions",
                "context_decisions",
                "context_events",
                "context_episodes",
                "context_subject_chapters",
                "context_chapters",
                "context_checkpoints",
                "outputs",
                "effects",
                "inputs",
                "runs",
                "messages",
                "sessions",
            ] {
                let column = if table == "sessions" {
                    "id"
                } else {
                    "session_id"
                };
                for session in &removed { tx.execute(
                    &format!("DELETE FROM {table} WHERE {column}=?1"),
                    [session],
                )?; }
            }
            for session in &removed {
                Self::delete_execution_mode_tx(tx, session)?;
                Self::clear_quota_wait_tx(tx, session, None)?;
            }
            Ok(())
        })?;
        for session in &removed {
            admission.remove(session);
            runtime.forget_history(session);
        }
        self.cleanup_deleted_sessions(&removed)?;
        Ok(())
    }
    pub fn fork(&self, session_id: &str, title: impl Into<String>) -> Result<Session> {
        self.fork_inner(session_id, None, title.into(), || Ok(()), |_| Ok(()))
    }
    /// Fork canonical history through `message_id`, including that message.
    /// The selected prefix must contain results for every tool call it retains.
    pub fn fork_at(
        &self,
        session_id: &str,
        message_id: &str,
        title: impl Into<String>,
    ) -> Result<Session> {
        self.fork_at_with(session_id, message_id, title, |_| Ok(()))
    }
    /// Fork and perform host work while source runtime admission remains excluded.
    /// The callback runs after the fork commits and may read Store or perform host
    /// work, but must not reenter runtime/admission operations (including fork,
    /// start, delete or compaction). Callback errors leave the fork committed.
    pub fn fork_at_with(
        &self,
        session_id: &str,
        message_id: &str,
        title: impl Into<String>,
        after: impl FnOnce(&Session) -> Result<()>,
    ) -> Result<Session> {
        self.fork_at_admitted(session_id, message_id, title, || Ok(()), after)
    }

    /// Like `fork_at_with`, but `admitted` runs once the source admission guard is
    /// held and before the fork is created. An error from it aborts the fork, so a
    /// check made there cannot be overtaken by a turn that starts in between.
    pub fn fork_at_admitted(
        &self,
        session_id: &str,
        message_id: &str,
        title: impl Into<String>,
        admitted: impl FnOnce() -> Result<()>,
        after: impl FnOnce(&Session) -> Result<()>,
    ) -> Result<Session> {
        self.fork_inner(session_id, Some(message_id), title.into(), admitted, after)
    }
    fn fork_inner(
        &self,
        session_id: &str,
        message_id: Option<&str>,
        title: String,
        admitted: impl FnOnce() -> Result<()>,
        after: impl FnOnce(&Session) -> Result<()>,
    ) -> Result<Session> {
        if self.0.layout.is_some() {
            return self.layout_fork(session_id, message_id, title, admitted, after);
        }
        let runtime = self.runtime();
        let _admission = runtime.fork_guard(session_id)?;
        admitted()?;
        self.session(session_id)?;
        let active_effect: bool = self.0.connection.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM effects WHERE session_id=?1 AND state='dispatched')",
            [session_id],
            |r| r.get(0),
        )?;
        anyhow::ensure!(!active_effect, "Cannot fork with active background effects");
        let source = Self::fork_history(self.messages(session_id)?, message_id)?;
        let strategy = self.session(session_id)?.context_strategy;
        let mut session = self.create_session_with_strategy(title, strategy)?;
        session.parent_session_id = Some(session_id.into());
        session.parent_cutoff = Some(source.len() as i64);
        let execution_mode = self.initial_execution_mode(Some(session_id))?;
        self.write(|tx| {
            Self::insert_execution_mode(tx, &session.id, &execution_mode)?;
            tx.execute(
                "UPDATE sessions SET data=?2 WHERE id=?1",
                params![session.id, serde_json::to_string(&session)?],
            )?;
            for mut msg in source {
                let original_message_id = msg.id.clone();
                msg.id = id();
                msg.session_id = session.id.clone();
                Self::insert_message(tx, &msg)?;
                Self::copy_reminder_metadata_tx(
                    tx,
                    session_id,
                    &original_message_id,
                    &session.id,
                    &msg.id,
                )?;
            }
            Ok(())
        })?;
        after(&session)?;
        Ok(session)
    }
    pub(super) fn fork_history(
        mut history: Vec<Message>,
        message_id: Option<&str>,
    ) -> Result<Vec<Message>> {
        if let Some(message_id) = message_id {
            let cutoff = history
                .iter()
                .position(|message| message.id == message_id)
                .context("Fork cutoff message is not in the source session")?;
            history.truncate(cutoff + 1);
        }
        anyhow::ensure!(
            !history
                .iter()
                .flat_map(|message| &message.tool_calls)
                .any(|call| !history
                    .iter()
                    .any(|message| message.tool_call_id.as_deref() == Some(&call.id))),
            "Cannot fork with unresolved tool calls"
        );
        Ok(history)
    }
}
