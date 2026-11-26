use crate::live_events::LiveEvents;
use crate::tool_gate::{GateCall, GateSource, ToolGate};
use crate::{
    CancellationToken, Observation, ObservationScope, OutputRecord, ToolCall, ToolContext,
    ToolRegistry, ToolResult, id, now,
};
use anyhow::{Result, anyhow, ensure};
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

mod calls;

/// Prepares a call context for its session; the shell builds it.
pub(crate) type ContextSetup = Arc<dyn Fn(&mut ToolContext) + Send + Sync>;

/// One dispatch path for direct model calls and host calls composed inside a cell.
/// Nested reads stay transient. Nested effects retain boundaries, without inventing
/// provider tool messages for calls that the provider did not make.
#[derive(Clone)]
pub struct ToolDispatcher {
    tools: ToolRegistry,
    journal: Arc<dyn crate::run_journal::RunJournal>,
    gate: Arc<dyn ToolGate>,
    live: Arc<dyn LiveEvents>,
    max_result_bytes: usize,
    observation: Option<Observation>,
    scope: ObservationScope,
    context_setup: ContextSetup,
    user_request: String,
    remaining_calls: Option<Arc<AtomicUsize>>,
    refresh_tools: Arc<AtomicBool>,
    interactive: bool,
    origin: &'static str,
    pub(crate) submitted_result: Arc<std::sync::Mutex<Option<Value>>>,
}

impl ToolDispatcher {
    /// A dispatcher over its three seams: the journal for durable writes, the
    /// gate that may ask the user and shapes results, and the sink for live
    /// events; plus the shell's hook that prepares each call context.
    pub(crate) fn from_seams(
        tools: ToolRegistry,
        max_result_bytes: usize,
        observation: Option<Observation>,
        journal: Arc<dyn crate::run_journal::RunJournal>,
        gate: Arc<dyn ToolGate>,
        live: Arc<dyn LiveEvents>,
        context_setup: ContextSetup,
    ) -> Self {
        Self {
            tools,
            journal,
            gate,
            live,
            max_result_bytes,
            observation,
            scope: ObservationScope::default(),
            context_setup,
            user_request: String::new(),
            remaining_calls: None,
            refresh_tools: Arc::default(),
            interactive: true,
            origin: "model",
            submitted_result: Arc::default(),
        }
    }
    /// Who is calling: `model` for a model tool call, `js` or `call_tool` for
    /// nested calls. Tools may pick a result shape for their caller.
    pub fn origin(&self) -> &'static str {
        self.origin
    }
    pub(crate) fn with_tool_budget(mut self, limit: Option<usize>) -> Self {
        self.remaining_calls = limit.map(|limit| Arc::new(AtomicUsize::new(limit)));
        self
    }
    pub(crate) fn with_interactive(mut self, enabled: bool) -> Self {
        self.interactive = enabled;
        self.origin = if enabled { "model" } else { "embedder" };
        self
    }
    /// Reserve before the gate and durable dispatch. Clones share the same run budget.
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
    /// Run the shell's hook that prepares a call context for its session before
    /// the gate sees it.
    pub(crate) fn prepare_context(&self, context: &mut ToolContext) {
        (self.context_setup)(context);
    }
    /// The context of a call the loop prepares for a run, without an invocation.
    pub(crate) fn call_context(
        &self,
        run: &crate::Run,
        cancellation: &CancellationToken,
    ) -> ToolContext {
        ToolContext {
            session_id: run.session_id.clone(),
            run_id: run.id.clone(),
            invocation_id: String::new(),
            cancellation: cancellation.clone(),
            extensions: Default::default(),
        }
    }
    /// The context of an output the loop emits itself, under the run's setup.
    pub(crate) fn notice_context(
        &self,
        run: &crate::Run,
        cancellation: &CancellationToken,
    ) -> ToolContext {
        let mut context = self.call_context(run, cancellation);
        self.prepare_context(&mut context);
        context
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
            interactive: self.interactive,
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
        self.journal.effect_completed(invocation)
    }
    /// Complete a background effect using its known session without restoring archives.
    pub fn complete_effect_for_session(&self, session: &str, invocation: &str) -> Result<()> {
        self.journal
            .effect_completed_for_session(session, invocation)
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
