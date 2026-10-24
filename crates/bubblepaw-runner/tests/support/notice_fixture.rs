//! A scripted tool whose queued texts reach the harness as one structured reminder per drain.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{Reminder, Tool, ToolContext, ToolDefinition, ToolResult};
use serde_json::{Value, json};
use std::sync::Mutex;

pub struct Notices {
    name: &'static str,
    pending: Mutex<Vec<String>>,
}
impl Notices {
    pub fn new(name: &'static str, texts: Vec<String>) -> Self {
        Self {
            name,
            pending: Mutex::new(texts),
        }
    }
}
#[async_trait]
impl Tool for Notices {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: "Fixture notice delivery".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        anyhow::bail!("fixture tool must not be invoked")
    }
    fn take_reminders(&self, _: &str) -> Vec<Reminder> {
        let texts = std::mem::take(&mut *self.pending.lock().unwrap());
        if texts.is_empty() {
            return Vec::new();
        }
        vec![Reminder::new(
            format!("host_tool:{}", self.name),
            texts.join("\n"),
        )]
    }
}
