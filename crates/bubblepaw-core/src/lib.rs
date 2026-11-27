//! A small native agent loop with durable history and effects, and transient presentation.
//!
//! Hosts register native capabilities with [`ToolRegistry::register_host`].
//! Native mode (the default) exposes those capabilities directly; JavaScript mode exposes only the registered `js` composition adapter.
//! Tool policies and effect boundaries apply equally to both modes.
//!
//! ```no_run
//! # use bubblepaw_core::*;
//! # async fn example(harness: &Harness, session_id: &str) -> anyhow::Result<()> {
//! let config = HarnessConfig {
//!     execution_mode: ExecutionMode::Native,
//!     ..Default::default()
//! };
//! let options = RunOptions::default();
//! let definitions = harness.effective_tool_definitions(session_id, &options)?;
//! let result: ToolResult = harness.call_tool(
//!     session_id, "read", serde_json::json!({"path":"README.md"}),
//!     options, CancellationToken::new(),
//! ).await?;
//! # Ok(()) }
//! ```
mod approvals;
mod cache_accounting;
#[doc(hidden)]
pub mod config_schema;
mod context_facts;
mod context_state;
mod decision;
mod event_log;
mod events;
mod host_state;
mod journal;
mod journal_admission;
mod journal_sqlite;
mod live_log;
mod observation;
mod provider_ext;
mod quota;
mod request_metering;
mod request_usage;
mod runtime;
mod store;
pub mod transcript;
// The run loop and the tool dispatcher live in `bubblepaw-agent`; the modules are
// aliased here so the shell's code keeps its `crate::tool::...` paths.
pub use approvals::{
    Approval, ApprovalMode, ApprovalPolicy, ApprovalRecord, ApprovalState, AutoApprovalConfig,
    DEFAULT_AUTO_PROMPT, Decision, exec_working_directory, track_approval_waits,
};
pub use bubblepaw_agent::{
    ExecutionMode, Extensions, OutputRecord, Run, SCRIPT_MODEL, Tool, ToolContext, ToolDispatcher,
    ToolRegistry, ToolResult, ToolResultLimiter, search_tool_definitions_with_servers,
    tool_definition_summary,
};
pub(crate) use bubblepaw_agent::{
    dispatch, live_events, loop_hooks, loop_options, loop_records, observer, prepare_types,
    reminder_types, request_seam, round_reminders, run_inputs, run_journal, submit_result,
    tool_gate, tool_source, tool_views,
};
pub use bubblepaw_ai::DEFAULT_CACHE_BLOCK_TOKENS;
pub use bubblepaw_ai::{
    Branch, BranchView, CacheMode, CancellationToken, Capabilities, ContentBlock, CursorResets,
    EPHEMERAL_ID_PREFIX, HistoryCursor, ImageRef, IncompleteReason, InputFileRef, JsonSchema,
    LastRequestUsage, MAX_IMAGE_BYTES, MAX_IMAGES_PER_CALL, Message, MessageInput, MessageOrigin,
    ModelInfo, ModelRequest, ModelResponse, PROVIDER_STATE_ORIGIN_FIELD, Provider, ProviderError,
    ProviderErrorKind, QuotaReading, QuotaSource, QuotaWait, ReasoningEffort, ReasoningReplay,
    RequestKind, Role, ServedRoute, StopReason, StreamCallback, StreamEvent, ToolCall,
    ToolDefinition, ToolSurface, Usage, WireShape, content_body_text, content_files,
    content_images, content_text, content_thinking, file_envelope, is_ephemeral, is_typed_user,
    plain_content, prepared_request_bytes, provider_error_retryable,
    provider_state_replay_compatible, reply_blocks, text_blocks, text_callback,
    with_provider_state_origin, with_text,
};
pub(crate) use bubblepaw_ai::{
    HistoryTracker, extra_content, prepared_message_bytes, upgrade_user_content,
};
pub use cache_accounting::{CacheAccounting, CacheOutcome, PrefixBreak, request_components};
pub use context_facts::{
    ContextBlock, ContextBlockKind, ContextEvent, ContextEventKind, ContextSurfaceRebind,
    ContextTrigger, MessageTokenSource, MessageTokens, ReportedMessageTokens,
};
pub use context_state::{
    BuiltContext, CacheTiers, ContextBuild, ContextClassification, ContextState, ContextWriter,
    Reminder, ReminderDelivery, ReminderMetadata, ReminderRecord, RequestLogRow, TierFingerprint,
};
pub use decision::{
    Answer, AnswerKind, DecisionChoice, DecisionProvider, DecisionRequest, DecisionResponse,
    DecisionUsage, Question, QuestionKind, ScoreLevel,
};
pub use event_log::{ClientInfo, EVENTS_AFTER_LIMIT, LogPosition, client_param};
pub use events::{EventKind, HarnessEvent};
pub use observation::{
    Observation, ObservationMode, ObservationRecord, ObservationScope, ObservationSink,
    fingerprint, hex_lower,
};
pub use provider_ext::{apply_tool_surface, derived_registry_resolver};
pub use quota::{QuotaWaitChanged, clear_current_quota_wait, set_quota_wait};
pub use request_usage::{
    RequestUsage, capture_request_usage, capture_request_usage_with_store, current_request_kind,
    discard_request_usage_since, record_request_usage, record_request_usage_with_output_presence,
    request_usage_count, with_request_usage_account, with_request_usage_provider,
};
pub use runtime::{
    ApprovalResolver, ContextBudgetExceeded, ContextPreparation, FullHistory, Harness,
    HarnessConfig, PrepareCall, PrepareMode, PreparedContext, ProviderResolver,
    RUNTIME_INVENTORY_HEADER, SessionBusy, SystemPromptResolver, ToolRegistryResolver, TurnStart,
    WorkspaceResolver,
};
use serde::{Deserialize, Serialize};
pub use store::{
    CHECKPOINT_PREFIX, ChapterSection, CheckpointRecord, ContextChapter, ContextCheckpoint,
    ContextDecision, ContextEpisode, ContextSubjectChapter, ContextView, DecisionKind,
    EpisodeBatch, InputKind, QueuedInput, RelatedEvidence, RootStoragePolicy, SearchHit,
    SearchIndexProgress, SessionCleanup, SessionRecord, SessionSnapshot, StorageStats, Store,
    StoreError, StoreOptions, StoreSchemaError, SubjectChapterStatus,
};
pub use transcript::{
    ImportDerivedRecord, ImportFormat, ImportOptions, ImportResult, ImportStats, import_file,
};

// The unit-test modules reach these through `crate::*` and `super::*`.
#[cfg(test)]
use {anyhow::Result, async_trait::async_trait, serde_json::Value, std::sync::Arc};

/// The host context policy selected when a session is created.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextStrategy {
    #[default]
    Lcm,
    Classical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub parent_session_id: Option<String>,
    pub parent_cutoff: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
    pub read_only: bool,
    /// Host-internal sessions remain addressable but are omitted from ordinary lists.
    #[serde(default)]
    pub hidden: bool,
    /// Missing in older archives, which use the existing LCM policy.
    #[serde(default)]
    pub context_strategy: ContextStrategy,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RunOptions {
    /// Origin of newly admitted input. Ordinary callers submit typed user input.
    #[serde(default)]
    pub origin: MessageOrigin,
    pub model: Option<String>,
    /// Override the harness model/tool round limit for this run. None inherits
    /// the harness limit, which is unlimited by default. Positive limits only.
    #[serde(default)]
    pub max_rounds: Option<usize>,
    pub system_prompt: Option<String>,
    /// Model entrypoints this run may call; other calls get an error result at call
    /// time. Declarations are not filtered. Host capabilities retain their host policy.
    pub tool_names: Option<Vec<String>>,
    pub max_output_tokens: Option<u32>,
    /// Required structured result, submitted through the run's `submit_result` tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<JsonSchema>,
    /// Shared budget for model tool calls and nested host invocations. None is unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<usize>,
    /// Run this `js` code as the run's single tool round instead of calling a model.
    #[serde(default)]
    pub script: Option<String>,
    /// Run approval restrictions, or explicit authorization for `Harness::call_tool`.
    #[serde(default)]
    pub approval: Option<ApprovalPolicy>,
    /// The client starting the run; its RunStarted carries it as `origin`.
    /// Wire commands take it as a top-level `client`, not inside `options`.
    #[serde(skip)]
    pub client: Option<ClientInfo>,
}

pub(crate) fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
pub(crate) fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
#[cfg(test)]
mod live_tests;
#[cfg(test)]
mod native_execution_tests;
#[cfg(test)]
mod request_size_tests;
#[cfg(test)]
mod tests;
