//! Immutable per-session execution surfaces in the existing chat metadata table.
use super::*;
use crate::ExecutionMode;

const UNBOUND: &str = "unbound";

fn key(session: &str) -> String {
    format!("execution_mode:{session}")
}

fn mode(value: Option<&str>) -> Result<ExecutionMode> {
    match value {
        Some("native") => Ok(ExecutionMode::Native),
        Some("js") | None => Ok(ExecutionMode::Js),
        Some(value) => bail!("Invalid persisted execution mode: {value}"),
    }
}

impl Store {
    /// The mode a chat has, or would bind to with `configured`, without binding
    /// it. Read paths use this; run admission binds the mode.
    pub fn candidate_execution_mode(
        &self,
        session: &str,
        configured: ExecutionMode,
    ) -> Result<ExecutionMode> {
        let current = self.chat_metadata(session, &key(session))?;
        if current.as_deref() == Some(UNBOUND) {
            Ok(configured)
        } else {
            mode(current.as_deref())
        }
    }
    /// Resolve a session's mode, binding a newly started chat exactly once.
    /// Missing metadata belongs to a legacy JavaScript chat, even if it is empty.
    /// New sessions carry an unbound marker until their first harness surface is
    /// selected. The conditional update prevents concurrent hosts changing it.
    pub fn resolve_execution_mode(
        &self,
        session: &str,
        configured: ExecutionMode,
    ) -> Result<ExecutionMode> {
        if let Some(chat) = self.scoped(session)? {
            return chat.resolve_execution_mode(session, configured);
        }
        self.session(session)?;
        let key = key(session);
        let current = self.chat_metadata(session, &key)?;
        if current.as_deref() != Some(UNBOUND) {
            return mode(current.as_deref());
        }
        self.write(|tx| Self::bind_execution_mode_tx(tx, session, configured))
    }

    /// Run admission binds the mode in the same commit as its accepted input.
    pub(crate) fn bind_execution_mode_tx(
        tx: &Connection,
        session: &str,
        configured: ExecutionMode,
    ) -> Result<ExecutionMode> {
        let key = key(session);
        let current: Option<String> = tx
            .query_row("SELECT value FROM metadata WHERE key=?1", [&key], |row| {
                row.get(0)
            })
            .optional()?;
        if current.as_deref() == Some(UNBOUND) {
            let value = match configured {
                ExecutionMode::Native => "native",
                ExecutionMode::Js => "js",
            };
            tx.execute(
                "UPDATE metadata SET value=?2 WHERE key=?1 AND value='unbound'",
                params![key, value],
            )?;
            Ok(configured)
        } else {
            mode(current.as_deref())
        }
    }

    pub(super) fn delete_execution_mode_tx(tx: &Connection, session: &str) -> Result<()> {
        tx.execute(
            "DELETE FROM metadata WHERE key IN (?1,?2)",
            params![key(session), super::tool_surface::key(session)],
        )?;
        Ok(())
    }

    pub(super) fn initial_execution_mode(&self, parent: Option<&str>) -> Result<String> {
        match parent {
            Some(parent) => Ok(self
                .chat_metadata(parent, &key(parent))?
                .unwrap_or_else(|| "js".into())),
            None => Ok(UNBOUND.into()),
        }
    }

    pub(super) fn insert_execution_mode(tx: &Connection, session: &str, value: &str) -> Result<()> {
        tx.execute(
            "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
            params![key(session), value],
        )?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "store_execution_mode_tests.rs"]
mod tests;
