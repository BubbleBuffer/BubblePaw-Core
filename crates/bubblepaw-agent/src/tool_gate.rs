//! How the dispatcher asks whether a call may run, and how a finished result is
//! shaped before the model sees it. Each method is one question about one call;
//! the implementation owns every record, event and wait that answering needs.
use crate::tool::ToolContext;
use crate::tool_registry::ToolResultLimiter;
use crate::tool_result::ToolResult;
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_ai::{ToolCall, ToolDefinition};

/// Where a call came from: a model reply, or a host call composed inside another call.
#[derive(Clone, Copy)]
pub enum GateSource<'a> {
    Model { message_id: &'a str },
    Nested,
}

/// One call as plain data. The context carries the session, run, cancellation and
/// the policy the host resolved for this call.
pub struct GateCall<'a> {
    pub context: &'a ToolContext,
    pub call: &'a ToolCall,
    pub source: GateSource<'a>,
    /// False for model-free calls: nobody is there to answer a question.
    pub interactive: bool,
    /// The user's latest request, as captured when the run started its round.
    pub user_request: &'a str,
    /// The byte limit a model-facing result is held to.
    pub result_limit: usize,
    /// The host's hook for preserving a large result before the hard limit.
    pub result_hook: Option<&'a ToolResultLimiter>,
}

/// The answer to "may this call run?".
pub enum Verdict {
    Allow,
    /// The call does not run; the text is the error the model reads.
    Deny(String),
    /// The run was cancelled while the call waited.
    Cancelled,
}

#[async_trait]
pub trait ToolGate: Send + Sync {
    /// Whether the call is certain to run without anything being written or asked.
    /// Pure: the caller uses it to commit the reply and the dispatch together.
    fn admits_without_write(
        &self,
        call: &GateCall<'_>,
        definition: &ToolDefinition,
    ) -> Result<bool>;
    /// The decision for a validated call, before its effect boundary. It may wait
    /// for a person, and returns early when the call's cancellation fires.
    async fn before(&self, call: &GateCall<'_>, definition: &ToolDefinition) -> Result<Verdict>;
    /// The result as the model should receive it, after the call ran.
    fn after(&self, call: &GateCall<'_>, result: ToolResult) -> Result<ToolResult>;
}
