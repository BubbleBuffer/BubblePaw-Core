//! The small seams: the gate, the tools, live events and the observer.
use crate::live_events::{LiveEvent, LiveEvents};
use crate::loop_options::LoopOptions;
use crate::observer::{ObservationScope, Observer, ToolWatch};
use crate::round_reminders::{RunReminders, ToolsRound};
use crate::run_data::Run;
use crate::tool_gate::{GateCall, ToolGate, Verdict};
use crate::tool_registry::ToolRegistry;
use crate::tool_result::ToolResult;
use crate::tool_source::{RunTools, ToolSource, ToolSurface};
use anyhow::{Result, bail};
use async_trait::async_trait;
use bubblepaw_ai::{ModelRequest, ToolDefinition};
use serde_json::Value;
use std::sync::Mutex;

/// Every call runs, and results go to the model as they are.
pub struct AllowAll;

#[async_trait]
impl ToolGate for AllowAll {
    fn admits_without_write(
        &self,
        _call: &GateCall<'_>,
        _definition: &ToolDefinition,
    ) -> Result<bool> {
        Ok(true)
    }
    async fn before(&self, _call: &GateCall<'_>, _definition: &ToolDefinition) -> Result<Verdict> {
        Ok(Verdict::Allow)
    }
    fn after(&self, _call: &GateCall<'_>, result: ToolResult) -> Result<ToolResult> {
        Ok(result)
    }
}

/// The same registry and system prompt for every round of every run. No reminders
/// are queued, so tools are always open; a run that requires a result gets no
/// reminder before its second reply without one fails.
pub struct FixedTools {
    registry: ToolRegistry,
    system_prompt: String,
}

impl FixedTools {
    pub fn new(registry: ToolRegistry, system_prompt: impl Into<String>) -> Self {
        Self {
            registry,
            system_prompt: system_prompt.into(),
        }
    }
}

impl ToolSource for FixedTools {
    fn open_run(&self, _run: &Run, _options: &LoopOptions) -> Result<RunTools> {
        Ok(RunTools {
            tools: self.registry.clone(),
            surface: ToolSurface {
                system_prompt: self.system_prompt.clone(),
                declarations: self.registry.definitions(),
            },
            refresh_each_round: false,
        })
    }
    fn round_tools(&self, _run: &Run, _options: &LoopOptions) -> Result<ToolRegistry> {
        Ok(self.registry.clone())
    }
    fn surface_changed(
        &self,
        _run: &Run,
        _options: &LoopOptions,
        _tools: &mut ToolRegistry,
        _surface: &mut ToolSurface,
        _request: &mut ModelRequest,
    ) -> Result<bool> {
        Ok(false)
    }
    fn queue_round_reminders(
        &self,
        _run: &Run,
        _options: &LoopOptions,
        _remaining_calls: Option<usize>,
        _state: &mut RunReminders,
    ) -> ToolsRound {
        ToolsRound::Open
    }
    fn bound_result(&self, result: ToolResult, _remaining_calls: Option<usize>) -> ToolResult {
        result
    }
    fn result_required(&self, _run: &Run, nudged: &mut bool) -> Result<()> {
        if std::mem::replace(nudged, true) {
            bail!("The model replied without the required result");
        }
        Ok(())
    }
}

/// Live events kept in order until taken.
#[derive(Default)]
pub struct CollectedEvents {
    events: Mutex<Vec<LiveEvent>>,
}

impl CollectedEvents {
    pub fn new() -> Self {
        Self::default()
    }
    /// The events collected since the last call, oldest first.
    pub fn take(&self) -> Vec<LiveEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
    fn push(&self, event: LiveEvent) {
        self.events.lock().unwrap().push(event);
    }
}

impl LiveEvents for CollectedEvents {
    fn transient(&self, _session_id: &str, _run_id: &str, event: LiveEvent) {
        self.push(event);
    }
    fn progress(&self, _session_id: &str, _run_id: &str, event: LiveEvent) {
        self.push(event);
    }
    fn paragraph(&self, _session_id: &str, _run_id: &str, text: String) {
        self.push(LiveEvent::Paragraph { text });
    }
}

/// Live events dropped.
pub struct NoLiveEvents;

impl LiveEvents for NoLiveEvents {
    fn transient(&self, _session_id: &str, _run_id: &str, _event: LiveEvent) {}
    fn progress(&self, _session_id: &str, _run_id: &str, _event: LiveEvent) {}
    fn paragraph(&self, _session_id: &str, _run_id: &str, _text: String) {}
}

/// An observer that records nothing.
pub struct NoObserver;

struct NoWatch;

impl ToolWatch for NoWatch {
    fn invocation(&mut self, _id: &str) {}
    fn finish(&mut self, _result: &Result<ToolResult>) {}
}

impl Observer for NoObserver {
    fn emit(
        &self,
        _scope: ObservationScope,
        _kind: &str,
        _metrics: Value,
        _payload: Option<Value>,
    ) {
    }
    fn failure(
        &self,
        _scope: ObservationScope,
        _kind: &str,
        _elapsed_ms: u64,
        _category: &str,
        _error: &str,
    ) {
    }
    fn tool_started(
        &self,
        _scope: ObservationScope,
        _name: &str,
        _input: &Value,
        _nested: bool,
        _origin: &str,
    ) -> Box<dyn ToolWatch> {
        Box::new(NoWatch)
    }
}
