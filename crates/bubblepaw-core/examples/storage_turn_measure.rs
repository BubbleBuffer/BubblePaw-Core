//! Synthetic two-round turn with one tool call; metadata-only storage diagnostics.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, RunOptions,
    Store, TextCallback, Tool, ToolCall, ToolContext, ToolDefinition, ToolRegistry, ToolResult,
};
use serde_json::{Value, json};
use std::sync::Arc;

struct Scripted;
#[async_trait]
impl Provider for Scripted {
    async fn generate(
        &self,
        request: ModelRequest,
        _on_text: TextCallback,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(
            if request.messages.iter().any(|m| m.tool_call_id.is_some()) {
                ModelResponse {
                    text: "Synthetic finished reply.".into(),
                    ..Default::default()
                }
            } else {
                ModelResponse {
                    tool_calls: vec![ToolCall {
                        id: "measure-call".into(),
                        name: "payload".into(),
                        arguments: json!({}),
                    }],
                    ..Default::default()
                }
            },
        )
    }
}
struct Payload;
#[async_trait]
impl Tool for Payload {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "payload".into(),
            description: "Synthetic payload".into(),
            input_schema: json!({"type":"object"}),
            read_only: false,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, _context: ToolContext, _input: Value) -> Result<ToolResult> {
        Ok(ToolResult::text(
            "Synthetic payload with no private content.\n".repeat(2400),
        ))
    }
}
async fn measure(path: String) -> Result<()> {
    let store = Store::open_data_dir(&path)?;
    let session = store.create_session("storage turn measurement")?;
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Payload))?;
    let harness = Harness::new(
        store.clone(),
        Arc::new(Scripted),
        tools,
        HarnessConfig {
            max_tool_result_bytes: 128 * 1024,
            ..Default::default()
        },
    );
    let before = store.storage_stats()?;
    let began = std::time::Instant::now();
    let run = harness
        .prompt(
            &session.id,
            "Generate a synthetic payload",
            RunOptions::default(),
        )
        .await?;
    let run_seconds = began.elapsed().as_secs_f64();
    let event_started = std::time::Instant::now();
    let events = harness.events_after(0, Some(&session.id), 1000)?;
    let event_read_seconds = event_started.elapsed().as_secs_f64();
    let stats_started = std::time::Instant::now();
    let stats = store.storage_stats()?;
    let stats_seconds = stats_started.elapsed().as_secs_f64();
    let writes = stats.transaction_count - before.transaction_count;
    let chat = stats.chat_transaction_count - before.chat_transaction_count;
    let serving = stats.serving_transaction_count - before.serving_transaction_count;
    let catalog = stats.catalog_transaction_count - before.catalog_transaction_count;
    println!(
        "{}",
        json!({"status":run.status,"wall_seconds":began.elapsed().as_secs_f64(),
        "run_seconds":run_seconds,"event_read_seconds":event_read_seconds,"stats_seconds":stats_seconds,
        "layout":"chat-v1","reported_transaction_delta":writes,"durable_events":events.len(),
        "transactions":chat+serving+catalog,"chat_transactions":chat,"serving_transactions":serving,"catalog_transactions":catalog,
        "transaction_basis":"Successful application commits across chat, serving and catalog; schema initialization excluded; not fsync calls",
        "messages":stats.message_count,"runtime_database_bytes":stats.database_bytes,"runtime_wal_bytes":stats.wal_bytes})
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args().nth(1).expect("data directory required");
    let repeats: usize = std::env::args()
        .nth(2)
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(1);
    anyhow::ensure!(
        (1..=10).contains(&repeats),
        "Repeat count must be between 1 and 10"
    );
    for index in 0..repeats {
        let trial = if repeats == 1 {
            path.clone()
        } else {
            format!("{path}/trial-{index}")
        };
        measure(trial).await?;
    }
    Ok(())
}
