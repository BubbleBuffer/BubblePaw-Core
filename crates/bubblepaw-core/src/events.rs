//! Durable and transient session events shared by every host transport.
use crate::{
    ApprovalRecord, ApprovalState, ClientInfo, OutputRecord, QuotaWait, ToolCall, ToolResult, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessEvent {
    /// For a durable event its own log sequence; for a live one (`Paragraph`)
    /// the sequence of the last durable event before it (docs/session-api.md).
    pub sequence: u64,
    #[serde(default = "durable_default")]
    pub durable: bool,
    /// The client whose command caused this event, when it named itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ClientInfo>,
    pub session_id: String,
    pub run_id: String,
    pub created_at: String,
    #[serde(flatten)]
    pub kind: EventKind,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// A durable context fact committed with its owning gather or apply operation.
    ContextCompaction {
        event_id: String,
        kind: crate::ContextEventKind,
        trigger: crate::ContextTrigger,
    },
    /// Small host-owned durable state. The engine does not interpret its value.
    HostStateChanged {
        key: String,
        value: Value,
    },
    Output {
        record: OutputRecord,
    },
    SessionSettled {},
    RunStarted {
        model: String,
    },
    /// A visible related session crossed a durable run boundary.
    ChildRunStateChanged {
        child_session_id: String,
        state: String,
    },
    RunWaitingForQuota {
        wait: QuotaWait,
    },
    RunQuotaResumed {},
    Paragraph {
        text: String,
    },
    /// Transient boundary before each provider request, including correction retries.
    /// Hosts can reset partial response capture without writing another history row.
    ModelRequestStarted {},
    /// Live progress: one text batch as the provider streams it. Never stored,
    /// not part of `streaming_text`, and not counted in `live_seen`; the
    /// paragraph events stay the reply's own view.
    TextDelta {
        text: String,
    },
    ToolStarted {
        call: ToolCall,
    },
    ToolFinished {
        call_id: String,
        name: String,
        result: ToolResult,
    },
    SteeringAccepted {
        text: String,
    },
    FollowupQueued {
        text: String,
    },
    RunFinished {
        status: String,
        text: Option<String>,
        error: Option<String>,
        usage: Option<Usage>,
    },
    ApprovalRequested {
        approval: ApprovalRecord,
    },
    ApprovalResolved {
        approval_id: String,
        state: ApprovalState,
        reason: Option<String>,
    },
}
fn durable_default() -> bool {
    true
}
