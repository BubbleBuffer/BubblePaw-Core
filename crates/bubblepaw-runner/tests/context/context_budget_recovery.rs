//! Budget-overflow recovery through the real Harness. A prior applied view
//! (manual compaction) makes the cold tier real, so the overhead note is served
//! from the first build. Tool growth then passes the ordinary prepare gate,
//! and the overhead on the final request overflows it. Recovery re-prepares in
//! pressure mode and retries final assembly, without replaying the tool.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextBudgetExceeded, ContextPreparation, Harness, HarnessConfig, Message,
    ModelRequest, ModelResponse, PrepareCall, PrepareMode, PreparedContext, Provider, Role, Run,
    RunOptions, Store, TextCallback, Tool, ToolCall, ToolContext, ToolDefinition, ToolRegistry,
    ToolResult, Usage,
};
use bubblepaw_runner::{
    episode_models::{
        ChapterInput, ChapterResult, EpisodeModels, SplitInput, SplitResult, SummaryInput,
        SummaryResult,
    },
    episode_worker::EpisodeWorker,
    episodes::{EpisodeConfig, EpisodeSummaryInput},
    summarization::{LcmConfig, SummarizingContext, SummaryConfig},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Notify, time::timeout};

#[path = "context_budget_recovery/fixture.rs"]
mod fixture;

const PHYSICAL_CAP: usize = 60_000;
const OUTPUT_RESERVE: usize = 16_000;
// Admission threshold: the physical cap minus the output reserve, deducted once.
const INPUT_BUDGET: usize = PHYSICAL_CAP - OUTPUT_RESERVE;
// Prior applied view: two small rounds, compacted before the run.
const COVERED_ROUNDS: usize = 2;
const COVERED_ROUND_TOKENS: usize = 1_000;
// Raw, uncovered complete round that the pressure gather must summarize.
// The first input fits with the fixed overhead. Adding the legal tool result
// passes ordinary preparation but exceeds 44k after the cold note is placed.
// Assertions below verify those preconditions using the actual estimator.
const TAIL_TOKENS: usize = 22_000;
/// Default `max_tool_result_bytes` (runtime.rs). Larger results are replaced by
/// a size-limit summary before the request, so the tool output must fit.
const MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;
const TOOL_TOKENS: usize = 10_000;
const OVERHEAD_TOKENS: usize = 14_000;
const OVERHEAD_MARKER: &str = "overhead-marker";
const PROMPT: &str = "Read the bulky evidence, then finish.";
const SUMMARY: &str = "Retained task progress.";
const TIMEOUT: Duration = Duration::from_secs(30);

fn message(session: &str, id: String, role: Role, content: String) -> Message {
    Message {
        id,
        session_id: session.into(),
        role,
        origin: None,
        content,
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: "2026-10-23T00:00:00Z".into(),
    }
}

/// Text whose canonical estimate is about `tokens`, measured with the same
/// estimator the final admission uses, so sizes track the real counter.
fn sized(policy: &SummarizingContext, tokens: usize) -> String {
    const UNIT: &str = "alpha beta gamma delta ";
    let probe = |text: String| {
        policy.estimated_tokens(&ModelRequest {
            messages: vec![message("probe", "probe".into(), Role::User, text)],
            ..Default::default()
        })
    };
    let baseline = probe(String::new());
    let measured = probe(UNIT.repeat(1_000)).saturating_sub(baseline).max(1);
    UNIT.repeat(tokens * 1_000 / measured)
}

#[derive(Default)]
struct Scripted {
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Provider for Scripted {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let answered = request.messages.iter().any(|m| m.role == Role::Tool);
        self.requests.lock().unwrap().push(request);
        if answered {
            return Ok(ModelResponse {
                text: "Finished.".into(),
                ..Default::default()
            });
        }
        Ok(ModelResponse {
            tool_calls: vec![ToolCall {
                id: uuid::Uuid::new_v4().to_string(),
                name: "bulky".into(),
                arguments: json!({}),
            }],
            ..Default::default()
        })
    }
}

struct Bulky {
    output: String,
    invocations: AtomicUsize,
}
#[async_trait]
impl Tool for Bulky {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            approval: Default::default(),
            name: "bulky".into(),
            description: "Synthetic oversized structured read".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text(self.output.clone()))
    }
}

#[derive(Default)]
struct ScriptedModels {
    inputs: Mutex<Vec<SummaryInput>>,
    active: AtomicUsize,
    peak: AtomicUsize,
    pause_first: AtomicBool,
    entered: Notify,
    release: Notify,
}
struct ActiveCall<'a>(&'a AtomicUsize);
impl Drop for ActiveCall<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
#[async_trait]
impl EpisodeModels for ScriptedModels {
    async fn decide(&self, input: SplitInput) -> Result<SplitResult> {
        let mut probabilities = vec![0.0; input.options.len()];
        probabilities.push(1.0);
        Ok(SplitResult {
            choice: None,
            probabilities,
            model: "script".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
    async fn summarize(&self, input: SummaryInput) -> Result<SummaryResult> {
        self.inputs.lock().unwrap().push(input);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        let _active = ActiveCall(&self.active);
        self.entered.notify_one();
        if self.pause_first.swap(false, Ordering::SeqCst) {
            self.release.notified().await;
        }
        Ok(SummaryResult {
            title: "Progress".into(),
            summary: SUMMARY.into(),
            model: "script".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
    async fn close_chapter(&self, _: ChapterInput) -> Result<ChapterResult> {
        Ok(ChapterResult {
            description: "Task progress".into(),
            summary: SUMMARY.into(),
            related_ids: vec![],
            related_evidence: vec![],
            decisions: None,
            sections: None,
            model: "script".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
}

/// Stages the fixed overhead after the real preparation, as the runtime
/// inventory and cold notes do. It is staged on every ordinary and pressure
/// build, never on manual compaction, and everything else forwards to the
/// real policy.
struct Overhead {
    inner: Arc<SummarizingContext>,
    text: String,
    /// Largest prepared estimate, before overhead, among tool-result requests.
    prepared_tokens: AtomicUsize,
    /// Typed budget rejections from final admission.
    rejections: AtomicUsize,
    /// Set when a pressure-mode preparation begins, before its gather can run.
    pressure: Notify,
}
#[async_trait]
impl ContextPreparation for Overhead {
    fn project_history(&self, history: &[Message]) -> Result<Vec<Message>> {
        self.inner.project_history(history)
    }
    fn history_frontier(&self, session_id: &str) -> Result<Option<i64>> {
        self.inner.history_frontier(session_id)
    }
    async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
        let PrepareCall {
            session_id,
            request,
            provider,
            writer,
            mode,
            cancellation,
        } = call;
        if matches!(mode, PrepareMode::Pressure { .. }) {
            self.pressure.notify_one();
        }
        let stage = !matches!(mode, PrepareMode::Compact { .. });
        let prepared = self
            .inner
            .prepare(PrepareCall {
                session_id,
                request,
                provider,
                writer,
                mode,
                cancellation,
            })
            .await?;
        if stage && !self.text.is_empty() {
            if prepared
                .request
                .messages
                .iter()
                .any(|m| m.role == Role::Tool)
            {
                self.prepared_tokens.fetch_max(
                    self.inner.estimated_tokens(&prepared.request),
                    Ordering::SeqCst,
                );
            }
            writer.cold_note("overhead", &self.text);
        }
        Ok(prepared)
    }
    fn observe_request(
        &self,
        session_id: &str,
        request: &ModelRequest,
        provider: Arc<dyn Provider>,
    ) {
        self.inner.observe_request(session_id, request, provider);
    }
    fn validate_prepared_request(
        &self,
        session_id: &str,
        request: &ModelRequest,
        provider: Arc<dyn Provider>,
    ) -> Result<()> {
        let result = self
            .inner
            .validate_prepared_request(session_id, request, provider);
        if result
            .as_ref()
            .is_err_and(|error| error.is::<ContextBudgetExceeded>())
        {
            self.rejections.fetch_add(1, Ordering::SeqCst);
        }
        result
    }
    fn cold_message_indexes(&self, session_id: &str, request: &ModelRequest) -> Result<Vec<usize>> {
        self.inner.cold_message_indexes(session_id, request)
    }
    fn release_session(&self, session_id: &str) {
        self.inner.release_session(session_id);
    }
    fn observe_response(&self, session_id: &str, response: &ModelResponse) {
        self.inner.observe_response(session_id, response);
    }
    fn take_prefix_change(&self, session_id: &str) -> Option<String> {
        self.inner.take_prefix_change(session_id)
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    store: Store,
    harness: Harness,
    provider: Arc<Scripted>,
    policy: Arc<SummarizingContext>,
    models: Arc<ScriptedModels>,
    worker: Arc<EpisodeWorker>,
    overhead: Arc<Overhead>,
    bulky: Arc<Bulky>,
    session: String,
    /// Applied view frontier right after setup, before the run.
    covered_before: Option<i64>,
    /// Message count after setup; the pending-gather tests summarize up to it.
    total: usize,
}

/// Holds the session gate with a paused gather over the whole setup history, so
/// the run's pressure step must wait for it. Ordinary cadence would plan no
/// episode for the 22k tail (cadence is 24k), so this uses the explicit manual
/// path, which bypasses cadence. Returns the gather task once its summarize call
/// is in flight.
async fn start_pending_gather(fixture: &Fixture) -> Result<tokio::task::JoinHandle<Result<()>>> {
    fixture.models.pause_first.store(true, Ordering::SeqCst);
    let worker = fixture.worker.clone();
    let session = fixture.session.clone();
    let end = fixture.total;
    let gather = tokio::spawn(async move { worker.compact_until(&session, end, None).await });
    timeout(TIMEOUT, fixture.models.entered.notified()).await?;
    Ok(gather)
}

#[tokio::test]
async fn overflow_after_tool_growth_recovers_when_the_gather_was_not_started() -> Result<()> {
    let fixture = Fixture::new(OVERHEAD_TOKENS).await?;
    let gathers_before = fixture.models.inputs.lock().unwrap().len();
    let run = fixture.run(PROMPT).await?;
    fixture.assert_recovered(&run)?;
    assert!(
        fixture.models.inputs.lock().unwrap().len() > gathers_before,
        "recovery started the gather itself"
    );
    assert_eq!(fixture.models.peak.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn overflow_recovery_waits_for_a_pending_gather_on_the_session_gate() -> Result<()> {
    let fixture = Fixture::new(OVERHEAD_TOKENS).await?;
    let gather = start_pending_gather(&fixture).await?;
    let release = async {
        timeout(TIMEOUT, fixture.overhead.pressure.notified()).await?;
        assert_eq!(
            fixture.models.active.load(Ordering::SeqCst),
            1,
            "the pending gather still holds the session gate"
        );
        fixture.models.release.notify_one();
        Ok::<(), anyhow::Error>(())
    };
    let (run, released) = tokio::join!(fixture.run(PROMPT), release);
    released?;
    gather.await??;
    fixture.assert_recovered(&run?)?;
    assert_eq!(
        fixture.models.peak.load(Ordering::SeqCst),
        1,
        "gathers never overlap on the session"
    );
    Ok(())
}

#[tokio::test]
async fn pending_overflow_recovery_stays_cancellable_while_waiting_on_the_gate() -> Result<()> {
    let fixture = Fixture::new(OVERHEAD_TOKENS).await?;
    let gather = start_pending_gather(&fixture).await?;
    let cancel = async {
        timeout(TIMEOUT, fixture.overhead.pressure.notified()).await?;
        assert!(
            fixture.harness.cancel(&fixture.session),
            "the run is still active while recovery waits"
        );
        fixture.models.release.notify_one();
        Ok::<(), anyhow::Error>(())
    };
    let (run, cancelled) = tokio::join!(fixture.run(PROMPT), cancel);
    cancelled?;
    gather.await??;
    let run = run?;
    assert_eq!(run.status, "cancelled", "{:?}", run.error);
    assert_eq!(
        fixture.provider.requests.lock().unwrap().len(),
        1,
        "no provider continuation after cancellation"
    );
    assert_eq!(fixture.bulky.invocations.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn oversized_fixed_overhead_ends_with_the_budget_error_before_any_dispatch() -> Result<()> {
    // Overhead alone exceeds the budget, so the first build is refused before
    // the provider or the tool can run.
    let fixture = Fixture::new(INPUT_BUDGET + 5_000).await?;
    let run = fixture.run(PROMPT).await?;
    assert_eq!(run.status, "failed");
    let error = run.error.clone().unwrap_or_default();
    assert!(
        error.contains("exceeds the context budget"),
        "the existing budget error is kept: {error}"
    );
    assert!(fixture.overhead.rejections.load(Ordering::SeqCst) >= 1);
    assert!(
        fixture.provider.requests.lock().unwrap().is_empty(),
        "nothing is dispatched"
    );
    assert_eq!(fixture.bulky.invocations.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn oversized_original_task_ends_with_the_budget_error_before_dispatch() -> Result<()> {
    let fixture = Fixture::new(0).await?;
    let task = fixture.oversized_task();
    let run = fixture.run(&task).await?;
    assert_eq!(run.status, "failed");
    let error = run.error.clone().unwrap_or_default();
    assert!(
        error.contains("exceeds the context budget"),
        "the existing budget error is kept: {error}"
    );
    assert!(
        fixture.provider.requests.lock().unwrap().is_empty(),
        "nothing is dispatched"
    );
    assert_eq!(fixture.bulky.invocations.load(Ordering::SeqCst), 0);
    Ok(())
}
