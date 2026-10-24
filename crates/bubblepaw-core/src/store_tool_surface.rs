//! Immutable chat prefix and its replaceable, apply-time tool catalog.
use super::*;
use crate::ToolDefinition;

pub(crate) fn key(session: &str) -> String {
    format!("tool_surface:{session}")
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct CatalogTool {
    pub definition: ToolDefinition,
    pub deferred: bool,
    pub server: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ChatToolSurface {
    pub system_prompt: String,
    pub declarations: Vec<ToolDefinition>,
    pub initial_result: Option<ToolDefinition>,
    #[serde(default)]
    pub result_contract_changed: bool,
    pub initial_catalog: Vec<CatalogTool>,
    pub observed_catalog: Vec<CatalogTool>,
    pub inventory_notice: Option<String>,
    #[serde(default)]
    pub inventory_notice_base: Option<Vec<CatalogTool>>,
    pub cold_catalog: Option<String>,
}

impl Store {
    pub(crate) fn chat_tool_surface(&self, session: &str) -> Result<Option<ChatToolSurface>> {
        self.chat_metadata(session, &key(session))?
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }

    pub(crate) fn bind_chat_tool_surface_tx(
        tx: &Connection,
        session: &str,
        surface: &ChatToolSurface,
    ) -> Result<()> {
        tx.execute(
            "INSERT OR IGNORE INTO metadata VALUES(?1,?2)",
            params![key(session), serde_json::to_string(surface)?],
        )?;
        Ok(())
    }

    pub(crate) fn update_chat_tool_surface(
        &self,
        session: &str,
        update: impl FnOnce(&mut ChatToolSurface) -> Result<()>,
    ) -> Result<ChatToolSurface> {
        if let Some(chat) = self.scoped(session)? {
            return chat.update_chat_tool_surface(session, update);
        }
        self.write(|tx| {
            let encoded: String = tx.query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [key(session)],
                |row| row.get(0),
            )?;
            let mut surface = serde_json::from_str(&encoded)?;
            update(&mut surface)?;
            let next = serde_json::to_string(&surface)?;
            if next != encoded {
                tx.execute(
                    "UPDATE metadata SET value=?2 WHERE key=?1",
                    params![key(session), next],
                )?;
            }
            Ok(surface)
        })
    }

    pub(crate) fn bind_chat_tool_surface(
        &self,
        session: &str,
        surface: &ChatToolSurface,
    ) -> Result<ChatToolSurface> {
        if let Some(chat) = self.scoped(session)? {
            return chat.bind_chat_tool_surface(session, surface);
        }
        if self.chat_tool_surface(session)?.is_none() {
            self.write(|tx| Self::bind_chat_tool_surface_tx(tx, session, surface))?;
        }
        self.chat_tool_surface(session)?
            .context("Chat tool surface was not bound")
    }
}

impl Store {
    /// Set a chat-level metadata key on a chat store. The rewind sweep uses it to mark
    /// a chat that has no sessions yet, so a session created later can rewind.
    pub fn set_chat_metadata(&self, key: &str, value: &str) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO metadata VALUES(?1,?2)",
                params![key, value],
            )?;
            Ok(())
        })
    }
    /// Read one per-chat metadata value. Each key belongs to its writer.
    pub fn chat_metadata(&self, session_id: &str, key: &str) -> Result<Option<String>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.chat_metadata(session_id, key);
        }
        self.read(|conn| {
            Ok(conn
                .query_row("SELECT value FROM metadata WHERE key=?1", [key], |row| {
                    row.get(0)
                })
                .optional()?)
        })
    }
}
