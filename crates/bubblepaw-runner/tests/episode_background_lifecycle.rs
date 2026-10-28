#[path = "support/episode_fixture.rs"]
mod episode_fixture;
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, EventKind, HarnessEvent, Message, MessageOrigin, ModelRequest,
    ModelResponse, Provider, Role, Store, TextCallback, ToolCall, Usage,
};
use bubblepaw_runner::{
    episode_background::EpisodeBackground,
    episode_models::{
        ChapterInput, ChapterResult, EpisodeModels, SplitInput, SplitResult, SummaryInput,
        SummaryResult,
    },
    episode_worker::EpisodeWorker,
    summarization::{LcmConfig, SummarizingContext, SummaryConfig},
};
use episode_fixture::new_episode_worker;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, broadcast};

#[path = "common/hook.rs"]
mod hook;
use hook::prepare_fresh;

struct UnusedProvider;
#[async_trait]
impl Provider for UnusedProvider {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        panic!("background gathering must use the episode models");
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
            summary: "Retained task progress.".into(),
            model: "script".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
    async fn close_chapter(&self, _: ChapterInput) -> Result<ChapterResult> {
        Ok(ChapterResult {
            description: "Task progress".into(),
            summary: "Retained task progress.".into(),
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

fn message(session: &str, id: &str, role: Role, content: String) -> Message {
    Message {
        id: id.into(),
        session_id: session.into(),
        role,
        origin: None,
        content,
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: "2026-10-17T00:00:00Z".into(),
    }
}
fn long_child_history(session: &str) -> Vec<Message> {
    let mut task = message(
        session,
        &format!("{session}-task"),
        Role::User,
        "Investigate and implement the assigned task.".into(),
    );
    task.origin = Some(MessageOrigin::ChildTask);
    let mut history = vec![task];
    for index in 0..4 {
        let call_id = format!("{session}-call-{index}");
        let mut call = message(
            session,
            &format!("{session}-assistant-{index}"),
            Role::Assistant,
            format!("Working on phase {index}."),
        );
        call.tool_calls.push(ToolCall {
            id: call_id.clone(),
            name: "read".into(),
            arguments: serde_json::json!({"path":"evidence.txt"}),
        });
        history.push(call);
        let mut result = message(
            session,
            &format!("{session}-result-{index}"),
            Role::Tool,
            " datum".repeat(35_000),
        );
        result.tool_call_id = Some(call_id);
        history.push(result);
    }
    history
}
fn wake(events: &broadcast::Sender<HarnessEvent>, session: &str) -> Result<()> {
    events.send(HarnessEvent {
        sequence: 1,
        durable: true,
        origin: None,
        session_id: session.into(),
        run_id: String::new(),
        created_at: String::new(),
        kind: EventKind::SessionSettled {},
    })?;
    Ok(())
}
type Services = (Arc<SummarizingContext>, Arc<EpisodeWorker>);

fn services(store: &Store, models: Arc<ScriptedModels>) -> Result<Services> {
    let summary = Arc::new(SummarizingContext::new(
        store.clone(),
        Arc::new(UnusedProvider),
        SummaryConfig {
            threshold_cap: 1_000_000,
            lcm: LcmConfig {
                // Exercise gathering while children share the fixture's high
                // serving-pressure cap, before any view should be applied.
                child_compact_threshold_tokens: None,
                ..Default::default()
            },
            ..Default::default()
        },
        None,
    )?);
    let worker = new_episode_worker(store.clone(), models, summary.clone());
    summary.set_episode_worker(&worker);
    Ok((summary, worker))
}
type BackgroundServices = (
    EpisodeBackground,
    broadcast::Sender<HarnessEvent>,
    Arc<SummarizingContext>,
    Arc<EpisodeWorker>,
);

fn start(store: &Store, models: Arc<ScriptedModels>) -> Result<BackgroundServices> {
    let (summary, worker) = services(store, models)?;
    let (events, receiver) = broadcast::channel(128);
    let background = EpisodeBackground::start(
        receiver,
        store.clone(),
        summary.clone(),
        worker.clone(),
        hook::fixed_template(ModelRequest {
            model: "script".into(),
            ..Default::default()
        }),
    );
    Ok((background, events, summary, worker))
}
async fn wait_until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("scripted background condition timed out");
}

#[tokio::test]
async fn long_hidden_child_assignment_gathers_before_apply_with_shared_single_flight() -> Result<()>
{
    let store = Store::in_memory()?;
    let root = store.create_session("root")?;
    let hidden = store.create_session_with_visibility(
        "parallel worker",
        Default::default(),
        Some(&root.id),
        true,
    )?;
    let history = long_child_history(&hidden.id);
    store.append_batch(&history)?;
    let models = Arc::new(ScriptedModels::default());
    models.pause_first.store(true, Ordering::SeqCst);
    let (background, events, summary, worker) = start(&store, models.clone())?;
    let request = ModelRequest {
        model: "script".into(),
        messages: history.clone(),
        ..Default::default()
    };
    let before = prepare_fresh(
        &*summary,
        Arc::new(UnusedProvider),
        &hidden.id,
        request.clone(),
    )
    .await?
    .request;
    assert!(
        summary.estimated_tokens(&request) < 500_000,
        "history is below the old activation gate"
    );
    wake(&events, &hidden.id)?;
    tokio::time::timeout(Duration::from_secs(10), models.entered.notified()).await?;
    for _ in 0..32 {
        wake(&events, &hidden.id)?;
    }
    let catch_up = {
        let worker = worker.clone();
        let session = hidden.id.clone();
        tokio::spawn(async move { worker.precompute(&session).await })
    };
    models.release.notify_one();
    catch_up.await??;
    background.shutdown(Duration::from_secs(10)).await;
    let episodes = store.context_episodes(&hidden.id)?;
    assert!(
        episodes.len() >= 2,
        "one long child assignment makes multiple ready episodes"
    );
    assert_eq!(
        models.inputs.lock().unwrap().len(),
        episodes.len(),
        "coalesced events and catch-up share committed coverage"
    );
    assert_eq!(models.peak.load(Ordering::SeqCst), 1);
    assert!(store.context_view(&hidden.id)?.is_none());
    assert_eq!(store.messages(&hidden.id)?, history);
    let after = prepare_fresh(&*summary, Arc::new(UnusedProvider), &hidden.id, request)
        .await?
        .request;
    assert_eq!(serde_json::to_value(before)?, serde_json::to_value(after)?);
    assert_eq!(
        store.messages(&hidden.id)?[0].origin,
        Some(MessageOrigin::ChildTask)
    );
    Ok(())
}

#[tokio::test]
async fn hidden_worker_waits_for_out_of_order_tool_group_completion() -> Result<()> {
    let store = Store::in_memory()?;
    let session =
        store.create_session_with_visibility("parallel worker", Default::default(), None, true)?;
    let mut history = long_child_history(&session.id);
    history.truncate(3);
    let second_id = format!("{}-second-call", session.id);
    history[1].tool_calls.push(ToolCall {
        id: second_id.clone(),
        name: "read".into(),
        arguments: serde_json::json!({}),
    });
    history[2].tool_call_id = Some(second_id);
    // The second call returns first; the first call is still pending.
    store.append_batch(&history)?;
    let models = Arc::new(ScriptedModels::default());
    let (background, events, _, _) = start(&store, models.clone())?;
    wake(&events, &session.id)?;
    background.shutdown(Duration::from_secs(10)).await;
    assert!(models.inputs.lock().unwrap().is_empty());
    assert!(store.context_episodes(&session.id)?.is_empty());
    let mut first_result = message(
        &session.id,
        "late-first-result",
        Role::Tool,
        " datum".repeat(35_000),
    );
    first_result.tool_call_id = Some(history[1].tool_calls[0].id.clone());
    store.append_batch(&[first_result])?;
    let (background, events, _, _) = start(&store, models.clone())?;
    wake(&events, &session.id)?;
    background.shutdown(Duration::from_secs(10)).await;
    let episodes = store.context_episodes(&session.id)?;
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].last_message_id, "late-first-result");
    Ok(())
}

async fn abort_background(drop_host: bool) -> Result<()> {
    let store = Store::in_memory()?;
    let session =
        store.create_session_with_visibility("parallel worker", Default::default(), None, true)?;
    store.append_batch(&long_child_history(&session.id))?;
    let models = Arc::new(ScriptedModels::default());
    models.pause_first.store(true, Ordering::SeqCst);
    let (background, events, _, worker) = start(&store, models.clone())?;
    wake(&events, &session.id)?;
    tokio::time::timeout(Duration::from_secs(10), models.entered.notified()).await?;
    if drop_host {
        drop(background);
    } else {
        background.shutdown(Duration::from_millis(10)).await;
    }
    wait_until(|| models.active.load(Ordering::SeqCst) == 0).await;
    worker.wait_idle(&session.id).await;
    assert!(store.context_episodes(&session.id)?.is_empty());
    assert_eq!(models.inputs.lock().unwrap().len(), 1);
    Ok(())
}
#[tokio::test]
async fn shutdown_aborts_in_flight_hidden_summary() -> Result<()> {
    abort_background(false).await
}
#[tokio::test]
async fn dropping_host_aborts_in_flight_hidden_summary() -> Result<()> {
    abort_background(true).await
}

#[tokio::test]
async fn retention_cancels_hidden_summary_and_stale_events_do_not_restore_chat() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open(directory.path())?;
    let root = store.create_session("root")?;
    let hidden = store.create_session_with_visibility(
        "parallel worker",
        Default::default(),
        Some(&root.id),
        true,
    )?;
    store.append_batch(&long_child_history(&hidden.id))?;
    let models = Arc::new(ScriptedModels::default());
    models.pause_first.store(true, Ordering::SeqCst);
    let (background, events, _, _) = start(&store, models.clone())?;
    wake(&events, &hidden.id)?;
    tokio::time::timeout(Duration::from_secs(10), models.entered.notified()).await?;
    store.delete_session(&hidden.id)?;
    // Store retention cleanup cancels the call without another harness event.
    wait_until(|| models.active.load(Ordering::SeqCst) == 0).await;
    wake(&events, &hidden.id)?;
    background.shutdown(Duration::from_secs(10)).await;
    assert!(
        store
            .list_sessions()?
            .iter()
            .all(|session| session.id != hidden.id)
    );
    assert_eq!(models.inputs.lock().unwrap().len(), 1);
    drop(store);
    let reopened = Store::open(directory.path())?;
    assert!(
        reopened
            .list_sessions()?
            .iter()
            .all(|session| session.id != hidden.id)
    );
    Ok(())
}

#[tokio::test]
async fn cancelling_shutdown_future_aborts_hidden_summary() -> Result<()> {
    let store = Store::in_memory()?;
    let session =
        store.create_session_with_visibility("parallel worker", Default::default(), None, true)?;
    store.append_batch(&long_child_history(&session.id))?;
    let models = Arc::new(ScriptedModels::default());
    models.pause_first.store(true, Ordering::SeqCst);
    let (background, events, _, worker) = start(&store, models.clone())?;
    wake(&events, &session.id)?;
    tokio::time::timeout(Duration::from_secs(10), models.entered.notified()).await?;
    let shutdown = tokio::spawn(background.shutdown(Duration::from_secs(60)));
    tokio::task::yield_now().await;
    shutdown.abort();
    assert!(shutdown.await.unwrap_err().is_cancelled());
    wait_until(|| models.active.load(Ordering::SeqCst) == 0).await;
    worker.wait_idle(&session.id).await;
    assert!(store.context_episodes(&session.id)?.is_empty());
    Ok(())
}

// These tests exercise gathering/apply with deterministic episode-only models.
// Fork transport is covered separately by captured-request adapter tests.
