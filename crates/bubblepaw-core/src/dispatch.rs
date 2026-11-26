use crate::tool_gate::{GateCall, GateSource, ToolGate};
use crate::{
    CancellationToken, EventKind, Observation, ObservationScope, OutputRecord, Store, ToolCall,
    ToolContext, ToolRegistry, ToolResult, id, now,
};
use anyhow::{Result, anyhow, ensure};
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

mod calls;

/// One dispatch path for direct model calls and host calls composed inside a cell.
/// Nested reads stay transient. Nested effects retain boundaries, without inventing
/// provider tool messages for calls that the provider did not make.
#[derive(Clone)]
pub struct ToolDispatcher {
    store: Store,
    tools: ToolRegistry,
    log: Arc<crate::event_log::Log>,
    journal: Arc<dyn crate::journal::Journal>,
    gate: Arc<dyn ToolGate>,
    max_result_bytes: usize,
    observation: Option<Observation>,
    scope: ObservationScope,
    approval: Arc<crate::ApprovalPolicy>,
    approval_resolver: Option<crate::ApprovalResolver>,
    user_request: String,
    remaining_calls: Option<Arc<AtomicUsize>>,
    refresh_tools: Arc<AtomicBool>,
    interactive_approval: bool,
    origin: &'static str,
    pub(crate) submitted_result: Arc<std::sync::Mutex<Option<Value>>>,
}

impl ToolDispatcher {
    pub(crate) fn new(
        store: Store,
        tools: ToolRegistry,
        max_result_bytes: usize,
        observation: Option<Observation>,
    ) -> Self {
        let log = store.runtime().log.clone();
        let journal = Arc::new(crate::journal_sqlite::SqliteJournal::new(
            store.clone(),
            log.clone(),
        ));
        let gate = Arc::new(crate::approvals::StockGate::new(store.clone(), None, None));
        Self {
            store,
            tools,
            log,
            journal,
            gate,
            max_result_bytes,
            observation,
            scope: ObservationScope::default(),
            approval: Arc::default(),
            approval_resolver: None,
            user_request: String::new(),
            remaining_calls: None,
            refresh_tools: Arc::default(),
            interactive_approval: true,
            origin: "model",
            submitted_result: Arc::default(),
        }
    }
    /// Who is calling: `model` for a model tool call, `js` or `call_tool` for
    /// nested calls. Tools may pick a result shape for their caller.
    pub fn origin(&self) -> &'static str {
        self.origin
    }
    pub(crate) fn with_journal(mut self, journal: Arc<dyn crate::journal::Journal>) -> Self {
        self.journal = journal;
        self
    }
    pub(crate) fn with_gate(mut self, gate: Arc<dyn ToolGate>) -> Self {
        self.gate = gate;
        self
    }
    pub(crate) fn with_tool_budget(mut self, limit: Option<usize>) -> Self {
        self.remaining_calls = limit.map(|limit| Arc::new(AtomicUsize::new(limit)));
        self
    }
    pub(crate) fn with_interactive_approval(mut self, enabled: bool) -> Self {
        self.interactive_approval = enabled;
        self.origin = if enabled { "model" } else { "embedder" };
        self
    }
    /// Reserve before approval and durable dispatch. Clones share the same run budget.
    #[allow(deprecated)] // fetch_update supports the workspace's Rust 1.88 MSRV.
    pub(crate) fn reserve_tool_call(&self) -> bool {
        self.remaining_calls.as_ref().is_none_or(|remaining| {
            remaining
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    value.checked_sub(1)
                })
                .is_ok()
        })
    }
    /// Remaining calls in this run, after reservations already admitted.
    pub fn remaining_tool_calls(&self) -> Option<usize> {
        self.remaining_calls
            .as_ref()
            .map(|remaining| remaining.load(Ordering::Relaxed))
    }
    pub(crate) fn with_approval(mut self, policy: crate::ApprovalPolicy) -> Self {
        self.approval = Arc::new(policy);
        self
    }
    pub(crate) fn with_approval_resolver(
        mut self,
        resolve: Option<crate::ApprovalResolver>,
    ) -> Self {
        self.approval_resolver = resolve;
        self
    }
    pub(crate) fn approval_policy(&self, session_id: &str) -> crate::ApprovalPolicy {
        self.approval_resolver
            .as_ref()
            .map(|resolve| resolve(session_id))
            .unwrap_or_else(|| (*self.approval).clone())
    }
    /// Inherit this call's approval scope, preserving an explicit run override.
    /// A host may further restrict the returned policy for a nested session.
    pub fn approval_resolver(&self, session_id: &str) -> crate::ApprovalResolver {
        let resolve = self.approval_resolver.clone();
        let policy = self.approval.clone();
        let session_id = session_id.to_owned();
        Arc::new(move |_| {
            resolve
                .as_ref()
                .map(|resolve| resolve(&session_id))
                .unwrap_or_else(|| (*policy).clone())
        })
    }
    /// Request a fresh host registry at the next model round of this turn only.
    /// Existing calls and clones retain their snapshot until that boundary.
    pub fn refresh_tools_next_round(&self) {
        self.refresh_tools.store(true, Ordering::Release);
    }
    pub(crate) fn take_tools_refresh(&self) -> bool {
        self.refresh_tools.swap(false, Ordering::AcqRel)
    }
    pub(crate) fn with_tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = tools;
        self
    }
    pub(crate) fn with_user_request(mut self, user_request: String) -> Self {
        self.user_request = user_request;
        self
    }
    fn gate_call<'a>(
        &'a self,
        context: &'a ToolContext,
        call: &'a ToolCall,
        source: GateSource<'a>,
    ) -> GateCall<'a> {
        GateCall {
            context,
            call,
            source,
            interactive: self.interactive_approval,
            user_request: &self.user_request,
            result_limit: self.max_result_bytes,
            result_hook: self.tools.result_limiter.as_ref(),
        }
    }
    /// Attribute host calls to the JS cell that owns them, including background work.
    pub fn with_execution(
        &self,
        id: impl Into<String>,
        environment: impl Into<String>,
        generation: u64,
    ) -> Self {
        let mut scoped = self.clone();
        scoped.scope.execution_id = Some(id.into());
        scoped.scope.environment = Some(environment.into());
        scoped.scope.generation = Some(generation);
        scoped
    }
    pub(crate) fn with_request(&self, request: Option<String>) -> Self {
        let mut scoped = self.clone();
        scoped.scope.model_request_id = request;
        scoped
    }
    fn observe_tool(
        &self,
        context: &ToolContext,
        call_id: String,
        name: &str,
        input: &Value,
        nested: bool,
    ) -> Option<crate::observation::ToolObservation> {
        self.observation.as_ref().map(|observer| {
            let mut scope = self.scope.clone();
            scope.session_id = context.session_id.clone();
            scope.run_id = context.run_id.clone();
            scope.call_id = Some(call_id);
            scope.invocation_id = if nested || context.invocation_id.is_empty() {
                None
            } else {
                Some(context.invocation_id.clone())
            };
            let origin = if nested && self.origin == "model" {
                "host"
            } else {
                self.origin
            };
            crate::observation::ToolObservation::start(
                observer.clone(),
                scope,
                name,
                input,
                nested,
                origin,
            )
        })
    }
    pub fn definitions(&self) -> Vec<crate::ToolDefinition> {
        self.tools.capabilities()
    }
    /// Original server identity for host discovery, independent of wire names.
    pub fn tool_server(&self, name: &str) -> Option<&str> {
        self.tools.tool_server(name)
    }
    /// Removed host names may retain callable error stubs without appearing in
    /// advertised definitions or discovery results.
    pub fn unavailable_names(&self) -> Vec<String> {
        self.tools.unavailable.keys().cloned().collect()
    }
    /// Whether a host capability's full signature is always shown.
    pub fn is_shown_always(&self, name: &str) -> bool {
        self.tools.is_shown_always(name)
    }
    pub fn emit_output(
        &self,
        context: &ToolContext,
        execution_id: &str,
        output: Value,
    ) -> Result<String> {
        ensure!(!context.cancellation.is_cancelled(), "Execution cancelled");
        let record = OutputRecord {
            id: id(),
            session_id: context.session_id.clone(),
            run_id: context.run_id.clone(),
            execution_id: execution_id.into(),
            created_at: now(),
            output,
        };
        self.journal.tool_output(&record)?;
        if let Some(observer) = &self.observation {
            let mut scope = self.scope.clone();
            scope.session_id = context.session_id.clone();
            scope.run_id = context.run_id.clone();
            scope.execution_id = Some(execution_id.into());
            let bytes = record.output.to_string();
            observer.emit(scope, "output_emitted", serde_json::json!({"output_type":record.output.get("type").and_then(Value::as_str),"status":record.output.get("status").and_then(Value::as_str),"output_bytes":bytes.len(),"fingerprint":crate::fingerprint(bytes.as_bytes())}), None);
        }
        let output_id = record.id.clone();
        Ok(output_id)
    }
    pub fn complete_effect(&self, invocation: &str) -> Result<()> {
        self.store.effect_completed(invocation)
    }
    /// Complete a background effect using its known session without restoring archives.
    pub fn complete_effect_for_session(&self, session: &str, invocation: &str) -> Result<()> {
        self.store.effect_completed_for_session(session, invocation)
    }
    fn limit_model_result(
        &self,
        gate_call: &GateCall<'_>,
        result: ToolResult,
    ) -> Result<ToolResult> {
        let result = self.gate.after(gate_call, result)?;
        if result.body().len() > self.max_result_bytes
            && let Ok(value) = serde_json::from_str::<Value>(&result.body())
        {
            let mut summary = serde_json::json!({"error":"Tool result exceeds size limit; request smaller output. Work already performed is not undone.","resultTruncated":true});
            for key in ["handle", "status", "cursor", "exitCode"] {
                if let Some(v) = value.get(key) {
                    summary[key] = v.clone();
                }
            }
            return Ok(ToolResult {
                content: crate::text_blocks(summary.to_string()),
                is_error: true,
                details: None,
                terminate: result.terminate,
                pending: result.pending,
            });
        }
        let mut result = result;
        result.bound_text(self.max_result_bytes);
        Ok(result)
    }
}
