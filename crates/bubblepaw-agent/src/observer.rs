//! What the run loop and the dispatcher ask of whoever watches a run. Each method
//! is one report named by what happened; the implementation decides what to record.
use crate::tool_result::ToolResult;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ObservationScope {
    pub session_id: String,
    pub run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
}

/// The watcher of a run: free-form reports, failures and the start of a tool call.
pub trait Observer: Send + Sync {
    /// A report of this kind with its metrics and, when contents are kept, a payload.
    fn emit(&self, scope: ObservationScope, kind: &str, metrics: Value, payload: Option<Value>);
    /// A failure of this kind, how long it took and its category.
    fn failure(
        &self,
        scope: ObservationScope,
        kind: &str,
        elapsed_ms: u64,
        category: &str,
        error: &str,
    );
    /// A tool call started. The watch it returns reports the call's end.
    fn tool_started(
        &self,
        scope: ObservationScope,
        name: &str,
        input: &Value,
        nested: bool,
        origin: &str,
    ) -> Box<dyn ToolWatch>;
}

/// One tool call being watched. A watch dropped before `finish` reports the call as abandoned.
pub trait ToolWatch: Send {
    /// The invocation id the call received.
    fn invocation(&mut self, id: &str);
    /// The call ended with this result.
    fn finish(&mut self, result: &Result<ToolResult>);
}
