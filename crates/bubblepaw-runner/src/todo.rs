//! The latest small todo list is host state, never an extra prompt.
use anyhow::{Result, ensure};
use async_trait::async_trait;
use bubblepaw_core::{Store, Tool, ToolContext, ToolDefinition, ToolRegistry, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

pub const MAX_TODO_ITEMS: usize = 50;
const MAX_TEXT_BYTES: usize = 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TodoItem {
    pub text: String,
    pub status: TodoStatus,
}

pub fn list(store: &Store, session: &str) -> Result<Vec<TodoItem>> {
    store
        .host_state(session, "todo")?
        .map(serde_json::from_value)
        .transpose()
        .map(|value| value.unwrap_or_default())
        .map_err(Into::into)
}

pub fn replace(store: &Store, context: &ToolContext, input: Value) -> Result<Value> {
    // The advertised shape is {items: [...]}: providers require object schemas for native
    // function tools. A bare array stays accepted for existing JavaScript callers.
    let input = match input {
        Value::Object(mut object) if object.len() == 1 && object.contains_key("items") => {
            object.remove("items").unwrap_or_default()
        }
        other => other,
    };
    let items: Vec<TodoItem> = serde_json::from_value(input)?;
    ensure!(
        items.len() <= MAX_TODO_ITEMS,
        "todo supports at most {MAX_TODO_ITEMS} items"
    );
    for item in &items {
        ensure!(
            !item.text.trim().is_empty() && item.text.len() <= MAX_TEXT_BYTES,
            "todo text must be nonempty and at most {MAX_TEXT_BYTES} bytes"
        );
    }
    store.update_host_state_snapshot(&context.session_id, &context.run_id, "todo", |_| {
        Ok(serde_json::to_value(items)?)
    })
}

pub fn register(registry: &mut ToolRegistry, store: Store) -> Result<()> {
    registry.register_host(Arc::new(TodoTool(store)))
}

struct TodoTool(Store);
#[async_trait]
impl Tool for TodoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "todo".into(),
            description: "Replace the entire todo list for this chat. Returns the saved list; at most 50 items, each with text and pending, in_progress or done status. Use {items: []} to clear.".into(),
            input_schema: json!({"type":"object","properties":{"items":{"type":"array","maxItems":MAX_TODO_ITEMS,"items":{"type":"object","properties":{"text":{"type":"string","minLength":1,"maxLength":MAX_TEXT_BYTES},"status":{"type":"string","enum":["pending","in_progress","done"]}},"required":["text","status"],"additionalProperties":false}}},"required":["items"],"additionalProperties":false}),
            // Session-local bookkeeping has no external effect to replay. It
            // remains available in read-only workspace profiles.
            read_only: true,
            approval: Default::default(),
        }
    }

    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        if context.cancellation.is_cancelled() {
            return Ok(ToolResult::error("Execution cancelled"));
        }
        Ok(match replace(&self.0, &context, input) {
            Ok(value) => ToolResult::text(value.to_string()),
            Err(error) => ToolResult::error(error.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ScriptedProvider;
    use bubblepaw_core::{CancellationToken, EventKind, Harness, HarnessConfig};

    fn context(session: &str) -> ToolContext {
        ToolContext {
            session_id: session.into(),
            run_id: "todo-test".into(),
            invocation_id: "todo-test".into(),
            cancellation: CancellationToken::new(),
            approval: Default::default(),
        }
    }

    #[test]
    fn replaces_caps_and_emits() -> Result<()> {
        let store = Store::in_memory()?;
        let harness = Harness::new(
            store.clone(),
            Arc::new(ScriptedProvider::new("done")),
            ToolRegistry::new(),
            HarnessConfig::default(),
        );
        let session = store.create_session("todo")?;
        let context = context(&session.id);
        let first = json!([{"text":"one","status":"pending"}]);
        assert_eq!(replace(&store, &context, first.clone())?, first);
        let next = json!([{"text":"two","status":"done"}]);
        replace(&store, &context, next.clone())?;
        assert_eq!(serde_json::to_value(list(&store, &session.id)?)?, next);
        let too_many = json!(vec![
            json!({"text":"item","status":"pending"});
            MAX_TODO_ITEMS + 1
        ]);
        assert!(replace(&store, &context, too_many).is_err());
        assert_eq!(list(&store, &session.id)?.len(), 1);
        let events = harness.events_after(0, Some(&session.id), 100)?;
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[1].kind, EventKind::HostStateChanged { key, value } if key == "todo" && value == &next)
        );
        replace(&store, &context, json!([]))?;
        assert!(list(&store, &session.id)?.is_empty());
        Ok(())
    }

    #[test]
    fn advertises_an_object_schema_and_accepts_both_shapes() -> Result<()> {
        let store = Store::in_memory()?;
        let definition = TodoTool(store.clone()).definition();
        assert_eq!(definition.input_schema["type"], "object");
        let session = store.create_session("todo")?;
        let context = context(&session.id);
        let items = json!([{"text":"wrapped","status":"in_progress"}]);
        assert_eq!(
            replace(&store, &context, json!({"items": items.clone()}))?,
            items
        );
        assert_eq!(replace(&store, &context, items.clone())?, items);
        assert!(replace(&store, &context, json!({"items": items, "extra": 1})).is_err());
        Ok(())
    }

    #[test]
    fn todo_is_durable_bounded_and_measured() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let session;
        {
            let store = Store::open_data_dir(directory.path())?;
            session = store.create_session("durable todo")?.id;
            let context = context(&session);
            store.idle_checkpoint()?;
            let before = store.storage_stats()?;
            let mut peak_wal = before.wal_bytes;
            for iteration in 0..100 {
                replace(
                    &store,
                    &context,
                    json!([{"text":format!("item {iteration}"),"status":"pending"}]),
                )?;
                peak_wal = peak_wal.max(store.storage_stats()?.wal_bytes);
            }
            let after = store.storage_stats()?;
            println!(
                "control todo 100 replacements: db {} -> {} bytes; WAL {} -> {} bytes; transactions total={} chat={} serving={} catalog={}",
                before.database_bytes,
                after.database_bytes,
                before.wal_bytes,
                after.wal_bytes,
                after.transaction_count - before.transaction_count,
                after.chat_transaction_count - before.chat_transaction_count,
                after.serving_transaction_count - before.serving_transaction_count,
                after.catalog_transaction_count - before.catalog_transaction_count
            );
            assert_eq!(list(&store, &session)?[0].text, "item 99");
            assert_eq!(
                after.chat_transaction_count - before.chat_transaction_count,
                100
            );
            assert_eq!(store.messages(&session)?.len(), 0);
            let database = directory.path().join("chats").join(format!("{session}.db"));
            let connection = rusqlite::Connection::open(database)?;
            let (rows, bytes): (i64, i64) = connection.query_row("SELECT count(*),coalesce(sum(length(value)),0) FROM host_state WHERE session_id=?1", [&session], |row| Ok((row.get(0)?,row.get(1)?)))?;
            assert_eq!(rows, 1);
            println!("control todo authoritative latest state: {rows} row, {bytes} bytes");
            let (snapshots, blobs, raw, compressed): (i64, i64, i64, i64) = connection.query_row(
                "SELECT count(*),count(DISTINCT a.blob_hash),coalesce(sum(b.raw_bytes),0),coalesce(sum(length(b.data)),0) FROM attachments a JOIN blobs b ON b.hash=a.blob_hash WHERE a.session_id=?1 AND a.id LIKE 'host-state-v1/%'",
                [&session], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
            assert_eq!((snapshots, blobs), (100, 100));
            println!(
                "control todo history: snapshots={snapshots} blobs={blobs} raw={raw} compressed={compressed} sampled_peak_wal={peak_wal}"
            );
            store.idle_checkpoint()?;
            let settled = store.storage_stats()?;
            println!(
                "control todo after idle checkpoint: db={} WAL={} bytes; transactions total={} chat={} serving={} catalog={}",
                settled.database_bytes,
                settled.wal_bytes,
                settled.transaction_count - before.transaction_count,
                settled.chat_transaction_count - before.chat_transaction_count,
                settled.serving_transaction_count - before.serving_transaction_count,
                settled.catalog_transaction_count - before.catalog_transaction_count
            );
        }
        let store = Store::open_data_dir(directory.path())?;
        assert_eq!(list(&store, &session)?[0].text, "item 99");
        store.delete_session(&session)?;
        Ok(())
    }
}
