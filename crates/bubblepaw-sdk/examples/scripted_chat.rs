//! A scripted chat: one host tool, one answer, no network.
//!
//! Run with `cargo run -p bubblepaw-sdk --example scripted_chat`.

use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_sdk::HostBuilder;
use bubblepaw_sdk::approvals::Approval;
use bubblepaw_sdk::harness::{HarnessConfig, RunOptions};
use bubblepaw_sdk::providers::{ProviderRegistry, ScriptedProvider, ScriptedStep};
use bubblepaw_sdk::store::Store;
use bubblepaw_sdk::tools::{Tool, ToolCall, ToolContext, ToolDefinition, ToolRegistry, ToolResult};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// A tiny host tool: it returns the text it was given and records the call.
struct Echo {
    ran: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(),
            description: "Return the given text unchanged.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
                "additionalProperties": false
            }),
            read_only: true,
            approval: Approval::None,
        }
    }

    async fn invoke(&self, _context: ToolContext, input: Value) -> Result<ToolResult> {
        self.ran.store(true, Ordering::SeqCst);
        let text = input
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        Ok(ToolResult::text(text))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session("scripted chat")?;

    let ran = Arc::new(AtomicBool::new(false));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Echo { ran: ran.clone() }))?;

    let steps = vec![ScriptedStep::Calls {
        calls: vec![ToolCall {
            id: "call-echo-1".into(),
            name: "echo".into(),
            arguments: json!({"text": "hello from the host tool"}),
        }],
    }];
    let providers = ProviderRegistry::new(
        "scripted",
        Arc::new(ScriptedProvider::with_steps(
            steps,
            "The echo tool answered.",
        )),
    );

    let host = HostBuilder::new(store.clone(), providers)
        .workspace(workspace.path())
        .tools(tools)
        .config(HarnessConfig {
            model: "scripted".into(),
            ..Default::default()
        })
        .build()?;

    host.harness
        .start(&session.id, "Use the echo tool.", RunOptions::default())?;
    let run = host.harness.wait_for_idle(&session.id).await?;
    println!("{}", run.text.unwrap_or_default());
    assert!(ran.load(Ordering::SeqCst), "the echo tool did not run");
    Ok(())
}
