//! A small native agent loop with durable history and effects, and transient presentation.
//!
//! Hosts register native capabilities with [`ToolRegistry::register_host`].
//! Native mode exposes those capabilities directly and is the default.
//! JavaScript mode exposes only the registered `js` composition adapter.
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
mod discovery;
mod dispatch;
mod event_log;
mod events;
mod host_state;
mod observation;
mod output_schema;
mod provider_ext;
mod quota;
mod reasoning;
mod request_size;
mod request_usage;
mod runtime;
mod script;
mod store;
mod stream;
mod submit_result;
mod tool_notices;
mod tool_views;
pub mod transcript;
use anyhow::{Context, Result, ensure};
pub use approvals::{
    Approval, ApprovalMode, ApprovalPolicy, ApprovalRecord, ApprovalState, AutoApprovalConfig,
    DEFAULT_AUTO_PROMPT, Decision, track_approval_waits,
};
use async_trait::async_trait;
pub use cache_accounting::{
    CacheAccounting, CacheOutcome, DEFAULT_CACHE_BLOCK_TOKENS, PrefixBreak, RequestKind, WireShape,
    request_components,
};
pub use context_facts::{
    ContextBlock, ContextBlockKind, ContextEvent, ContextEventKind, ContextSurfaceRebind,
    ContextTrigger, MessageTokenSource, MessageTokens, ReportedMessageTokens,
};
pub use context_state::{
    BuiltContext, CacheTiers, ContextBuild, ContextClassification, ContextState, ContextWriter,
    LegacyReminderSource, Reminder, ReminderDelivery, ReminderMetadata, ReminderRecord,
    RequestLogRow, TierFingerprint,
};
pub use decision::{
    Answer, AnswerKind, DecisionChoice, DecisionProvider, DecisionRequest, DecisionResponse,
    DecisionUsage, Question, QuestionKind, ScoreLevel,
};
pub use discovery::{search_tool_definitions_with_servers, tool_definition_summary};
pub use dispatch::ToolDispatcher;
pub use event_log::{ClientInfo, EVENTS_AFTER_LIMIT, LogPosition, client_param};
pub use events::{EventKind, HarnessEvent};
pub use observation::{
    Observation, ObservationMode, ObservationRecord, ObservationScope, ObservationSink, fingerprint,
};
pub use output_schema::JsonSchema;
pub use provider_ext::{
    CacheMode, Capabilities, CursorResets, EPHEMERAL_ID_PREFIX, HistoryCursor, ProviderError,
    ProviderErrorKind, ToolSurface, apply_tool_surface, derived_registry_resolver, is_ephemeral,
    provider_error_retryable,
};
pub use quota::{
    QuotaReading, QuotaSource, QuotaWait, QuotaWaitChanged, clear_current_quota_wait,
    set_quota_wait,
};
pub use reasoning::{
    PROVIDER_STATE_ORIGIN_FIELD, ReasoningReplay, provider_state_replay_compatible,
    with_provider_state_origin,
};
pub use request_size::prepared_request_bytes;
pub use request_usage::{
    LastRequestUsage, RequestUsage, capture_request_usage, capture_request_usage_with_store,
    current_request_kind, discard_request_usage_since, record_request_usage,
    record_request_usage_with_output_presence, request_usage_count, with_request_usage_account,
    with_request_usage_provider,
};
pub use runtime::{
    ApprovalResolver, ContextBudgetExceeded, ContextPreparation, FullHistory, Harness,
    HarnessConfig, PrepareCall, PrepareMode, PreparedContext, ProviderResolver,
    RUNTIME_INVENTORY_HEADER, SessionBusy, SystemPromptResolver, ToolRegistryResolver, TurnStart,
    WorkspaceResolver,
};
pub use script::SCRIPT_MODEL;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};
pub use store::{
    CHECKPOINT_PREFIX, ChapterSection, ChatOpenFailure, ChatOpenHook, CheckpointRecord,
    ContextChapter, ContextCheckpoint, ContextDecision, ContextEpisode, ContextSubjectChapter,
    ContextView, DecisionKind, EpisodeBatch, InputKind, LegacyCheckpointNote, QueuedInput,
    RelatedEvidence, RootStoragePolicy, SearchHit, SearchIndexProgress, SessionCleanup,
    SessionRecord, SessionSnapshot, StorageStats, Store, StoreOptions, StoreSchemaError,
    SubjectChapterStatus,
};
use tokio::sync::watch;
pub use transcript::{
    ImportDerivedRecord, ImportFormat, ImportOptions, ImportResult, ImportStats, import_file,
};

pub type TextCallback = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone, Debug)]
pub struct CancellationToken(Arc<watch::Sender<bool>>);
impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}
impl CancellationToken {
    pub fn new() -> Self {
        let (sender, _) = watch::channel(false);
        Self(Arc::new(sender))
    }
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    pub async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        while !*receiver.borrow_and_update() {
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Message provenance, independent of its attachment envelope.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrigin {
    #[default]
    Typed,
    ChildTask,
    Generated,
    /// Replay-only neutral marker; the archive contains no recorded tool outcome.
    ArchiveUnknownOutcome,
}

/// Legacy child-task wrapper prefixes. Explicit origins take precedence.
pub const GENERATED_USER_WRAPPER_PREFIXES: &[&str] = &["Task:\n", "Explicit context (JSON):\n"];

pub fn is_typed_user(message: &Message) -> bool {
    message.role == Role::User
        && match message.origin {
            Some(MessageOrigin::Typed) => true,
            Some(
                MessageOrigin::ChildTask
                | MessageOrigin::Generated
                | MessageOrigin::ArchiveUnknownOutcome,
            ) => false,
            None => !GENERATED_USER_WRAPPER_PREFIXES
                .iter()
                .any(|prefix| message.content.trim_start().starts_with(prefix)),
        }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub id: String,
    pub session_id: String,
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_state: Option<Value>,
    #[serde(default)]
    pub is_error: bool,
    pub created_at: String,
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub session_id: String,
    pub model: String,
    pub status: String,
    pub text: Option<String>,
    pub error: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub read_only: bool,
    /// Whether calls wait for a person's decision (docs/approvals.md).
    #[serde(default)]
    pub approval: Approval,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
    /// The effect has started but is still owned by the host. Its implementation
    /// settles the boundary when the asynchronous operation actually terminates.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
}
/// Explicit presentation output. The core stores data, never frontend code or binary payloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputRecord {
    pub id: String,
    pub session_id: String,
    pub run_id: String,
    pub execution_id: String,
    pub created_at: String,
    pub output: Value,
}
impl ToolResult {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            pending: false,
        }
    }
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            pending: false,
        }
    }
    pub fn pending(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            pending: true,
        }
    }
}
#[derive(Clone, Debug)]
pub struct ToolContext {
    pub session_id: String,
    pub run_id: String,
    pub invocation_id: String,
    pub cancellation: CancellationToken,
    /// Effective run policy; tools may inspect it to constrain delegated work.
    pub approval: ApprovalPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Cached tokens are subsets of input tokens, not additional input totals.
    #[serde(default, deserialize_with = "deserialize_cache_read_tokens")]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
    /// Reasoning tokens, a subset of output tokens, when the provider reports
    /// them. `None` means not reported; it is omitted when serialized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}
// Earlier archives used Option<u64>, serializing explicit unknown values as null.
fn deserialize_cache_read_tokens<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<u64>::deserialize(deserializer)?.unwrap_or(0))
}
impl Usage {
    /// Accumulate another request's usage into run totals.
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(other.cache_creation_tokens);
        self.reasoning_tokens = match (self.reasoning_tokens, other.reasoning_tokens) {
            (Some(a), Some(b)) => Some(a.saturating_add(b)),
            (a, b) => a.or(b),
        };
    }
}
/// Requested reasoning effort. An unspecified model request uses the provider's
/// low default; it never means reasoning is disabled.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    #[default]
    Low,
    Medium,
    High,
    XHigh,
    Max,
}
impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelRequest {
    pub model: String,
    pub system_prompt: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    /// Inactive declarations retained for signed replay; unavailable for invocation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_tools: Vec<ToolDefinition>,
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Legacy input compatibility only; structured runs use `RunOptions::output_schema`.
    #[serde(default, skip_serializing)]
    pub output_schema: Option<JsonSchema>,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub history: HistoryCursor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_tiers: Option<CacheTiers>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelResponse {
    pub text: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quota: Vec<QuotaReading>,
    /// Distinguishes the final physical request from cumulative continuation usage.
    /// Absent metadata retains compatibility with providers returning one logical response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_request_usage: Option<LastRequestUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_state: Option<Value>,
}
impl ModelResponse {
    /// Deserialize a complete JSON reply into a host-owned type. This does not
    /// validate JSON Schema; serde enforces the destination type's contract.
    pub fn parse_json<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        ensure!(
            self.tool_calls.is_empty(),
            "cannot parse a tool-call response as a JSON reply"
        );
        serde_json::from_str(&self.text)
            .context("model reply is not valid JSON for the requested type")
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Ordered, explicitly configured routes for an isolated summary branch.
    /// This only describes candidates: it must not update main-session routing
    /// or dispatch a request. The captured successful main route comes first.
    fn summary_fork_routes(&self, model: &str) -> Vec<String> {
        vec![model.to_owned()]
    }
    /// Estimate one explicitly selected fork route, without including other
    /// possible main-request fallbacks in its admission calculation.
    fn estimate_summary_fork_tokens(
        &self,
        request: &ModelRequest,
        count: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        self.estimate_input_tokens(request, count)
    }
    /// Whether this exact route can append an isolated episode-summary branch
    /// while preserving the main request's native prefix and generation settings.
    fn supports_summary_fork(&self, _model: &str) -> bool {
        false
    }
    /// Canonical source IDs mapped to the actual rendered input positions.
    /// A missing projection cannot be silently replaced with bounded episode input.
    fn summary_fork_mapping(&self, _request: &ModelRequest) -> Result<Option<Value>> {
        Ok(None)
    }
    /// Dispatch an isolated branch. Never update a main-session mirror or route
    /// selection. Dispatch exactly one route; the caller owns explicit retries
    /// across `summary_fork_routes` while retaining the branch input shape.
    async fn generate_summary_fork(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        anyhow::bail!("Provider does not support isolated summary forks")
    }
    fn models(&self) -> Vec<ModelInfo> {
        Vec::new()
    }
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse>;
    /// Generate one JSON decision sample using the request's reasoning effort.
    async fn generate_decision(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        self.generate(request, on_text, cancellation).await
    }
    fn capabilities(&self, _model: &str) -> Capabilities {
        Capabilities::default()
    }
    /// Optional pure projection of model input for a host's local token estimate.
    /// Reuse the adapter's renderer; omit credentials and non-replayed archives.
    /// None leaves the host's canonical estimator in place. This is not a
    /// provider tokenizer or a network request.
    fn estimate_input_tokens(
        &self,
        _request: &ModelRequest,
        _count: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        Ok(None)
    }
    /// Optional pure projection of the exact wire body for prompt-cache
    /// accounting (see `WireShape`). None skips accounting for this request.
    fn wire_shape(&self, _request: &ModelRequest) -> Option<WireShape> {
        None
    }
    /// The session was deleted or moved to another provider; drop any mirror.
    async fn close_session(&self, _session_id: &str) -> Result<()> {
        Ok(())
    }
}
#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    /// The definition as advertised from a given registry. Tools whose
    /// description depends on the registry's current capabilities (`js`)
    /// override this; the result must be deterministic for the same registry.
    fn definition_in(&self, _registry: &ToolRegistry) -> ToolDefinition {
        self.definition()
    }
    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult>;
    async fn invoke_with_dispatcher(
        &self,
        context: ToolContext,
        input: Value,
        _dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        self.invoke(context, input).await
    }
    /// Explicit host control may release a foreground wait without cancelling work.
    fn interrupt_wait(&self, _session_id: &str) -> usize {
        0
    }
    /// Drain session-owned reminders for the next model request. Reminders
    /// keep their delivery identities through coalescing and recovery.
    fn take_reminders(&self, _session_id: &str) -> Vec<Reminder> {
        Vec::new()
    }
    fn cancel_session(&self, _session_id: &str) -> usize {
        0
    }
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    host_tools: BTreeMap<String, Arc<dyn Tool>>,
    promoted: std::collections::BTreeSet<String>,
    deferred: std::collections::BTreeSet<String>,
    tool_servers: BTreeMap<String, String>,
    validators: BTreeMap<String, Arc<jsonschema::Validator>>,
    shown_always: std::collections::BTreeSet<String>,
    unavailable: BTreeMap<String, String>,
    result_limiter: Option<ToolResultLimiter>,
    generation: u64,
    /// Model entrypoints a run may call; declarations are not affected.
    model_allowlist: Option<Vec<String>>,
}
/// Optional host policy for preserving large results before the engine's hard cap.
pub type ToolResultLimiter =
    Arc<dyn Fn(&Store, &ToolContext, ToolResult, usize) -> Result<ToolResult> + Send + Sync>;
impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set_result_limiter(&mut self, limiter: ToolResultLimiter) {
        self.result_limiter = Some(limiter);
    }
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<()> {
        self.register_tool(tool, false)
    }
    /// Register a host capability. It is callable through a model tool's dispatcher
    /// by default, and can also be exposed directly in native execution mode.
    pub fn register_host(&mut self, tool: Arc<dyn Tool>) -> Result<()> {
        self.register_tool(tool, true)
    }
    fn register_tool(&mut self, tool: Arc<dyn Tool>, host: bool) -> Result<()> {
        let definition = tool.definition();
        let name = definition.name;
        anyhow::ensure!(self.get(&name).is_none(), "Duplicate tool name: {name}");
        let validator = jsonschema::validator_for(&definition.input_schema)
            .map_err(|e| anyhow::anyhow!("Invalid tool schema for {name}: {e}"))?;
        self.unavailable.remove(&name);
        self.validators.insert(name.clone(), Arc::new(validator));
        if host {
            self.host_tools.insert(name, tool);
        } else {
            self.tools.insert(name, tool);
        }
        Ok(())
    }
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.get_model(name).or_else(|| self.get_host(name))
    }
    /// Explain a removed capability without advertising it as available.
    /// The host builds a replacement snapshot before publishing it.
    pub fn remove(&mut self, name: impl Into<String>, reason: impl Into<String>) {
        let name = name.into();
        self.tools.remove(&name);
        self.host_tools.remove(&name);
        self.promoted.remove(&name);
        self.deferred.remove(&name);
        self.tool_servers.remove(&name);
        self.validators.remove(&name);
        self.shown_always.remove(&name);
        self.unavailable.insert(name, reason.into());
    }
    pub(crate) fn unavailable_error(&self, name: &str, fallback: &str) -> anyhow::Error {
        match self.unavailable.get(name) {
            Some(reason) => anyhow::anyhow!("Tool {name} is unavailable: {reason}"),
            None => anyhow::anyhow!("{fallback}: {name}"),
        }
    }
    pub fn get_model(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned().or_else(|| {
            self.promoted
                .contains(name)
                .then(|| self.get_host(name))
                .flatten()
        })
    }
    pub fn get_host(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.host_tools.get(name).cloned()
    }
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .chain(
                self.promoted
                    .iter()
                    .filter_map(|name| self.host_tools.get(name)),
            )
            .map(|tool| tool.definition_in(self))
            .collect()
    }
    /// Mark host capabilities whose full call signatures are always shown in
    /// a code tool's description. Others are listed by name and discovered on
    /// demand. Names need not be registered yet.
    pub fn show_always<I, S>(&mut self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.shown_always.extend(names.into_iter().map(Into::into));
    }
    /// A registered host capability marked always-shown.
    pub fn is_shown_always(&self, name: &str) -> bool {
        self.shown_always.contains(name) && self.host_tools.contains_key(name)
    }
    /// Host capability definitions, including any also exposed in native mode.
    pub fn capabilities(&self) -> Vec<ToolDefinition> {
        self.host_tools
            .iter()
            .filter(|(name, _)| {
                self.model_allowlist.as_ref().is_none_or(|names| {
                    names.contains(name) || crate::tool_views::is_access_wrapper(name)
                })
            })
            .map(|(_, tool)| tool.definition())
            .collect()
    }
    pub(crate) fn validate(&self, name: &str, input: &Value) -> Result<()> {
        let tool = self
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Unknown tool: {name}"))?;
        let validator = self
            .validators
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Unknown tool: {name}"))?;
        validator.validate(input).map_err(|error| {
            // Prefer an actionable argument-name error even if a missing required
            // field or another invalid value was encountered first.
            let error = validator
                .iter_errors(input)
                .find(|error| {
                    matches!(
                        error.kind(),
                        jsonschema::error::ValidationErrorKind::AdditionalProperties { .. }
                    ) || (matches!(
                        error.kind(),
                        jsonschema::error::ValidationErrorKind::FalseSchema
                    ) && error
                        .schema_path()
                        .to_string()
                        .ends_with("/additionalProperties"))
                })
                .unwrap_or(error);
            let path = error.instance_path().to_string();
            let path = if path.is_empty() { "/" } else { &path };
            let schema = tool.definition().input_schema;
            let unexpected = match error.kind() {
                jsonschema::error::ValidationErrorKind::AdditionalProperties { unexpected } => {
                    Some(unexpected.clone())
                }
                // A closed object without `properties` uses a false-schema
                // validator. Its displayed error would otherwise echo a value.
                jsonschema::error::ValidationErrorKind::FalseSchema
                    if error
                        .schema_path()
                        .to_string()
                        .ends_with("/additionalProperties") =>
                {
                    input
                        .pointer(&error.instance_path().to_string())
                        .and_then(Value::as_object)
                        .map(|object| object.keys().cloned().collect())
                }
                _ => None,
            };
            if let Some(unexpected) = unexpected {
                let schema_path = error.schema_path().to_string();
                let object = schema_path
                    .strip_suffix("/additionalProperties")
                    .and_then(|path| schema.pointer(path));
                let valid: Vec<_> = object
                    .and_then(|object| object["properties"].as_object())
                    .into_iter()
                    .flat_map(|properties| properties.keys().map(String::as_str))
                    .collect();
                return anyhow::anyhow!(
                    "Invalid arguments for {name} at {path}: unexpected arguments: {}. Valid arguments: {}.",
                    unexpected.join(", "),
                    if valid.is_empty() {
                        "(none)".to_owned()
                    } else {
                        valid.join(", ")
                    }
                );
            }
            let choices = schema
                .pointer(&error.schema_path().to_string())
                .and_then(Value::as_array);
            let types: Vec<_> = choices
                .into_iter()
                .flatten()
                .filter_map(|choice| choice["type"].as_str())
                .take(8)
                .collect();
            let hint = if types.is_empty() {
                String::new()
            } else {
                format!(" Expected one of: {}.", types.join(" or "))
            };
            let detail: String = error.to_string().chars().take(512).collect();
            anyhow::anyhow!("Invalid arguments for {name} at {path}: {detail}.{hint}")
        })
    }
    /// Explicitly release a session's foreground waits without cancelling work.
    /// Ordinary steering queues input and does not invoke this host control.
    pub fn interrupt_waits(&self, session: &str) -> usize {
        self.tools
            .values()
            .chain(self.host_tools.values())
            .map(|tool| tool.interrupt_wait(session))
            .sum()
    }
}

/// How registered host capabilities are presented to a model.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// Offer only the registered `js` composition adapter to the model.
    Js,
    /// Offer registered native capabilities directly, without `js`.
    #[default]
    Native,
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
    /// Replaces the harness approval policy for this run.
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
mod tests;
