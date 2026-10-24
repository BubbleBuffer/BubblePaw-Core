//! Scripted runner/library acceptance. No paid or live model requests.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    Approval, CancellationToken, Harness, HarnessConfig, MessageOrigin, ModelRequest,
    ModelResponse, Provider, Role, RunOptions, Store, TextCallback, Tool, ToolCall, ToolContext,
    ToolDefinition, ToolDispatcher, ToolRegistry, ToolResult,
};
use bubblepaw_runner::{
    agents::{AgentProfile, AgentRegistry, AgentTools},
    config::Agents,
    tools::{ProcessTools, WorkspaceTools},
    wake::WakeCoordinator,
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, mpsc};

#[path = "auto_wake/exec.rs"]
mod exec;
use exec::*;

struct Probe {
    entered: mpsc::UnboundedSender<String>,
    release: Notify,
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Provider for Probe {
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let input = request
            .messages
            .iter()
            .rfind(|message| message.role == Role::User)
            .map(|message| message.content.clone())
            .unwrap_or_default();
        let has_tool = request
            .messages
            .iter()
            .any(|message| message.role == Role::Tool);
        self.requests.lock().unwrap().push(request.clone());
        if input == "fail" {
            anyhow::bail!("synthetic failure");
        }
        if input == "stream-timeout" {
            on_text("transient unfinished child fragment");
            self.entered.send(request.session_id.clone())?;
            cancellation.cancelled().await;
            anyhow::bail!("synthetic streaming cancellation");
        }
        if input == "gate" || input == "leaf" || input == "busy-tools" && !has_tool {
            self.entered.send(request.session_id.clone())?;
            tokio::select! { _ = cancellation.cancelled() => anyhow::bail!("cancelled"), _ = self.release.notified() => {} }
        }
        let tool = if input == "nested" && !has_tool {
            Some(("spawn", json!({"task":"leaf"})))
        } else if input == "busy-tools" && !has_tool {
            Some(("noop", json!({})))
        } else {
            None
        };
        Ok(match tool {
            Some((name, arguments)) => ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: name.into(),
                    arguments,
                }],
                ..Default::default()
            },
            None => ModelResponse {
                text: format!("answer:{input}"),
                ..Default::default()
            },
        })
    }
}
struct Noop;
struct Spawn;
#[async_trait]
impl Tool for Spawn {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "spawn".into(),
            description: "fixture child bridge".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Approval::None,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        unreachable!()
    }
    async fn invoke_with_dispatcher(
        &self,
        context: ToolContext,
        input: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        let result = dispatcher.invoke(context, "agent", input).await?;
        Ok(ToolResult {
            pending: false,
            ..result
        })
    }
}
#[async_trait]
impl Tool for Noop {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "noop".into(),
            description: "fixture".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Approval::None,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        Ok(ToolResult::text("ok"))
    }
}
struct Fixture {
    store: Store,
    root: Arc<Harness>,
    parent: String,
    agents: Arc<AgentTools>,
    probe: Arc<Probe>,
    entered: mpsc::UnboundedReceiver<String>,
    wake: Arc<WakeCoordinator>,
}
impl Fixture {
    fn new(enabled: bool, debounce: u64) -> Result<Self> {
        Self::limited(enabled, debounce, None)
    }
    fn limited(enabled: bool, debounce: u64, max_retained: Option<usize>) -> Result<Self> {
        let store = Store::in_memory()?;
        let parent = store.create_session("parent")?.id;
        let (entered_tx, entered) = mpsc::unbounded_channel();
        let probe = Arc::new(Probe {
            entered: entered_tx,
            release: Notify::new(),
            requests: Mutex::default(),
        });
        let wake = Arc::new(WakeCoordinator::new(
            enabled,
            Duration::from_millis(debounce),
        ));
        let registry = Arc::new(AgentRegistry::default().with_wake(wake.clone()));
        let config = Agents {
            auto_wake: enabled,
            wake_debounce_ms: debounce,
            max_retained,
            ..Default::default()
        };
        let agents =
            AgentTools::new_recursive(store.clone(), config, registry, None, None, |manager| {
                let mut tools = ToolRegistry::new();
                manager.register_weak(&mut tools)?;
                tools.register(Arc::new(Spawn))?;
                Ok(vec![AgentProfile {
                    name: "read-only".into(),
                    description: "fixture".into(),
                    harness: Arc::new(Harness::new(
                        store.clone(),
                        probe.clone(),
                        tools,
                        HarnessConfig::default(),
                    )),
                    model: None,
                    max_rounds: Some(12),
                    approval: None,
                }])
            })?;
        let mut tools = ToolRegistry::new();
        agents.register(&mut tools)?;
        tools.register(Arc::new(Noop))?;
        let root = Arc::new(Harness::new(
            store.clone(),
            probe.clone(),
            tools,
            HarnessConfig::default(),
        ));
        Ok(Self {
            store,
            root,
            parent,
            agents,
            probe,
            entered,
            wake,
        })
    }
    fn context(&self, owner: &str) -> ToolContext {
        ToolContext {
            session_id: owner.into(),
            run_id: "fixture".into(),
            invocation_id: "fixture".into(),
            cancellation: CancellationToken::new(),
            approval: Default::default(),
        }
    }
    fn start(&self, task: &str) -> Result<Value> {
        self.agents
            .start(&self.context(&self.parent), json!({"task":task}))
    }
    async fn entered(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(3), self.entered.recv())
            .await
            .unwrap()
            .unwrap()
    }
    fn notices(&self, session: &str) -> Vec<String> {
        self.store
            .messages(session)
            .unwrap()
            .into_iter()
            .filter(|message| {
                message.origin == Some(MessageOrigin::Generated)
                    && message.content.starts_with("<harness ")
            })
            .map(|message| message.content)
            .collect()
    }
}
async fn eventually(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(exec_test_timeout(), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
/// Waits until the wake driver has passed every notice published so far.
/// The driver admits all due owners in one pass, so once a later required
/// notice for a fresh visible root has a run, any earlier deliverable notice
/// would have one too: a missing run is then observed, not a timing guess.
async fn wake_barrier(store: &Store, wake: &WakeCoordinator) -> Result<()> {
    let sentinel = store.create_session("wake barrier")?.id;
    wake.publish_required(&sentinel, "barrier", "barrier".into());
    eventually(|| !store.runs(&sentinel).unwrap().is_empty()).await;
    Ok(())
}
#[tokio::test]
async fn idle_children_batch_into_one_durable_generated_turn() -> Result<()> {
    let fixture = Fixture::new(true, 60)?;
    let _registration = fixture.wake.register_harness(fixture.root.clone());
    let _driver = fixture.wake.start_driver();
    let first = fixture.start("first")?;
    let second = fixture.start("second")?;
    eventually(|| fixture.notices(&fixture.parent).len() == 1).await;
    fixture.root.wait_for_idle(&fixture.parent).await?;
    let notices = fixture.notices(&fixture.parent);
    assert!(notices[0].contains(first["sessionId"].as_str().unwrap()));
    assert!(notices[0].contains(second["sessionId"].as_str().unwrap()));
    assert_eq!(fixture.store.runs(&fixture.parent)?.len(), 1);
    assert!(fixture.agents.take_reminders(&fixture.parent).is_empty());
    Ok(())
}
#[tokio::test]
async fn busy_model_request_consumes_completion_without_an_extra_turn() -> Result<()> {
    let mut fixture = Fixture::new(true, 20)?;
    let _registration = fixture.wake.register_harness(fixture.root.clone());
    let _driver = fixture.wake.start_driver();
    fixture
        .root
        .start(&fixture.parent, "busy-tools", RunOptions::default())?;
    assert_eq!(fixture.entered().await, fixture.parent);
    let child = fixture.start("child")?;
    eventually(|| {
        fixture.agents.status(&fixture.parent, &json!({})).unwrap()["children"][0]["status"]
            == "idle"
    })
    .await;
    wake_barrier(&fixture.store, &fixture.wake).await?;
    fixture.probe.release.notify_waiters();
    fixture.root.wait_for_idle(&fixture.parent).await?;
    wake_barrier(&fixture.store, &fixture.wake).await?;
    assert_eq!(fixture.store.runs(&fixture.parent)?.len(), 1);
    assert_eq!(fixture.notices(&fixture.parent).len(), 1);
    assert!(fixture.notices(&fixture.parent)[0].contains(child["sessionId"].as_str().unwrap()));
    Ok(())
}
#[tokio::test]
async fn busy_final_response_leaves_notice_for_idle_wake() -> Result<()> {
    let mut fixture = Fixture::new(true, 20)?;
    let _registration = fixture.wake.register_harness(fixture.root.clone());
    let _driver = fixture.wake.start_driver();
    fixture
        .root
        .start(&fixture.parent, "gate", RunOptions::default())?;
    fixture.entered().await;
    fixture.start("child")?;
    eventually(|| {
        fixture.agents.status(&fixture.parent, &json!({})).unwrap()["children"][0]["status"]
            == "idle"
    })
    .await;
    fixture.probe.release.notify_waiters();
    eventually(|| fixture.store.runs(&fixture.parent).unwrap().len() == 2).await;
    fixture.root.wait_for_idle(&fixture.parent).await?;
    assert_eq!(fixture.notices(&fixture.parent).len(), 1);
    Ok(())
}
#[tokio::test]
async fn disabled_and_stopped_parents_wait_for_accepted_user_input() -> Result<()> {
    for enabled in [false, true] {
        let fixture = Fixture::new(enabled, 10)?;
        let _registration = fixture.wake.register_harness(fixture.root.clone());
        let _driver = fixture.wake.start_driver();
        if enabled {
            fixture.wake.suppress(&fixture.parent);
        }
        fixture.start("child")?;
        eventually(|| {
            fixture.agents.status(&fixture.parent, &json!({})).unwrap()["children"][0]["status"]
                == "idle"
        })
        .await;
        wake_barrier(&fixture.store, &fixture.wake).await?;
        assert!(fixture.store.runs(&fixture.parent)?.is_empty());
        fixture
            .root
            .start(&fixture.parent, "typed", RunOptions::default())?;
        fixture.wake.user_input(&fixture.parent);
        fixture.root.wait_for_idle(&fixture.parent).await?;
        assert_eq!(fixture.notices(&fixture.parent).len(), 1);
        wake_barrier(&fixture.store, &fixture.wake).await?;
        assert_eq!(fixture.store.runs(&fixture.parent)?.len(), 1);
    }
    Ok(())
}
#[tokio::test]
async fn direct_result_read_cancels_pending_wake() -> Result<()> {
    let fixture = Fixture::new(true, 150)?;
    let _registration = fixture.wake.register_harness(fixture.root.clone());
    let _driver = fixture.wake.start_driver();
    let child = fixture.start("child")?;
    fixture
        .agents
        .wait(
            &fixture.parent,
            child["handle"].as_str().unwrap(),
            Some(3000),
        )
        .await?;
    wake_barrier(&fixture.store, &fixture.wake).await?;
    assert!(fixture.store.runs(&fixture.parent)?.is_empty());
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_reads_racing_child_auto_turns_consume_the_matching_completion() -> Result<()> {
    let fixture = Fixture::new(true, 0)?;
    fixture.wake.suppress(&fixture.parent);
    let _registration = fixture.wake.register_harness(fixture.root.clone());
    let _driver = fixture.wake.start_driver();
    let child = fixture.start("child")?;
    let child_id = child["sessionId"].as_str().unwrap().to_owned();
    fixture
        .agents
        .wait(&fixture.parent, &child_id, Some(3000))
        .await?;
    for index in 0..20 {
        fixture.wake.publish(
            &child_id,
            format!("process:synthetic-{index}"),
            "Synthetic job ended.".into(),
        );
        let agents = fixture.agents.clone();
        let parent = fixture.parent.clone();
        let address = child_id.clone();
        let reads = tokio::spawn(async move {
            for _ in 0..30 {
                agents.status(&parent, &json!({"sessionId":address}))?;
                tokio::task::yield_now().await;
            }
            Ok::<_, anyhow::Error>(())
        });
        eventually(|| fixture.store.runs(&child_id).unwrap().len() == index + 2).await;
        reads.await??;
        fixture
            .agents
            .wait(&fixture.parent, &child_id, Some(3000))
            .await?;
        assert!(fixture.agents.take_reminders(&fixture.parent).is_empty());
    }
    assert!(fixture.store.runs(&fixture.parent)?.is_empty());
    Ok(())
}
#[tokio::test]
async fn nested_visible_parent_wakes_through_child_observer_and_remains_continuable() -> Result<()>
{
    let mut fixture = Fixture::new(true, 15)?;
    let _registration = fixture.wake.register_harness(fixture.root.clone());
    let _driver = fixture.wake.start_driver();
    let parent = fixture.start("nested")?;
    let leaf = fixture.entered().await;
    let child_id = parent["sessionId"].as_str().unwrap();
    assert_ne!(leaf, child_id);
    eventually(|| {
        fixture.agents.status(&fixture.parent, &json!({})).unwrap()["children"]
            .as_array()
            .unwrap()
            .iter()
            .any(|child| child["sessionId"] == child_id && child["status"] == "idle")
    })
    .await;
    fixture.probe.release.notify_waiters();
    eventually(|| fixture.store.runs(child_id).unwrap().len() == 2).await;
    eventually(|| {
        fixture.agents.status(&fixture.parent, &json!({})).unwrap()["children"]
            .as_array()
            .unwrap()
            .iter()
            .any(|child| child["sessionId"] == child_id && child["status"] == "idle")
    })
    .await;
    assert_eq!(fixture.notices(child_id).len(), 1);
    fixture.agents.send(
        &fixture.context(&fixture.parent),
        json!({"sessionId":child_id,"text":"follow-up"}),
    )?;
    fixture
        .agents
        .wait(
            &fixture.parent,
            parent["handle"].as_str().unwrap(),
            Some(3000),
        )
        .await?;
    assert_eq!(fixture.store.runs(child_id)?.len(), 3);
    Ok(())
}
#[tokio::test]
async fn failed_and_cancelled_children_deliver_terminal_notices() -> Result<()> {
    let mut fixture = Fixture::new(true, 20)?;
    let _registration = fixture.wake.register_harness(fixture.root.clone());
    let _driver = fixture.wake.start_driver();
    let failed = fixture.start("fail")?;
    eventually(|| fixture.notices(&fixture.parent).len() == 1).await;
    assert!(fixture.notices(&fixture.parent)[0].contains("status failed"));
    fixture.root.wait_for_idle(&fixture.parent).await?;
    let cancelled = fixture.start("gate")?;
    fixture.entered().await;
    fixture
        .agents
        .cancel(&fixture.parent, cancelled["handle"].as_str().unwrap())
        .await?;
    eventually(|| fixture.notices(&fixture.parent).len() == 2).await;
    assert!(fixture.notices(&fixture.parent)[1].contains("status cancelled"));
    assert_ne!(failed["sessionId"], cancelled["sessionId"]);
    Ok(())
}
#[tokio::test]
async fn hidden_worker_and_its_descendants_never_wake_visible_parent() -> Result<()> {
    let mut fixture = Fixture::new(true, 10)?;
    let _registration = fixture.wake.register_harness(fixture.root.clone());
    let _driver = fixture.wake.start_driver();
    let profile = fixture.agents.profile("read-only").unwrap().clone();
    let worker = fixture.agents.start_worker(
        &fixture.context(&fixture.parent),
        profile,
        json!({"task":"nested"}),
    )?;
    let descendant = fixture.entered().await;
    assert_ne!(descendant, worker["sessionId"].as_str().unwrap());
    assert!(fixture.store.session(&descendant)?.hidden);
    fixture.probe.release.notify_waiters();
    let worker = worker["sessionId"].as_str().unwrap();
    eventually(|| {
        fixture
            .store
            .runs(worker)
            .unwrap()
            .last()
            .is_some_and(|run| run.status == "completed")
    })
    .await;
    wake_barrier(&fixture.store, &fixture.wake).await?;
    assert!(fixture.store.runs(&fixture.parent)?.is_empty());
    assert!(fixture.agents.take_reminders(&fixture.parent).is_empty());
    Ok(())
}
#[tokio::test]
async fn idle_and_busy_background_exec_use_the_same_completion_inbox() -> Result<()> {
    for busy in [false, true] {
        let mut fixture = Fixture::new(true, 20)?;
        let workspace = tempfile::tempdir()?;
        let processes = Arc::new(ProcessTools::new(WorkspaceTools::new(workspace.path())?));
        processes.set_wake(fixture.wake.clone());
        processes.set_store(fixture.store.clone());
        let mut tools = ToolRegistry::new();
        processes.register(&mut tools)?;
        tools.register(Arc::new(Noop))?;
        fixture.root = Arc::new(Harness::new(
            fixture.store.clone(),
            fixture.probe.clone(),
            tools.clone(),
            HarnessConfig::default(),
        ));
        let _registration = fixture.wake.register_harness(fixture.root.clone());
        let _driver = fixture.wake.start_driver();
        if busy {
            fixture
                .root
                .start(&fixture.parent, "busy-tools", RunOptions::default())?;
            fixture.entered().await;
        }
        let receipt = tools
            .get("exec")
            .unwrap()
            .invoke(
                fixture.context(&fixture.parent),
                json!({"command":completed_command(),"background":true}),
            )
            .await?;
        let receipt: Value = serde_json::from_str(&receipt.content)?;
        assert_completed_exec(&fixture, &tools, &receipt["handle"]).await?;
        if busy {
            fixture.probe.release.notify_waiters();
        }
        eventually(|| fixture.notices(&fixture.parent).len() == 1).await;
        fixture.root.wait_for_idle(&fixture.parent).await?;
        assert_eq!(fixture.store.runs(&fixture.parent)?.len(), 1);
        assert!(fixture.notices(&fixture.parent)[0].contains(receipt["handle"].as_str().unwrap()));
        assert!(
            fixture.notices(&fixture.parent)[0]
                .contains("exited (exit code 0); stdout 9 bytes, stderr 0 bytes")
        );
        assert!(processes.take_reminders(&fixture.parent).is_empty());
        processes.shutdown().await;
    }
    Ok(())
}
#[tokio::test]
async fn mixed_child_and_exec_completions_batch_into_one_parent_turn() -> Result<()> {
    let mut fixture = Fixture::new(true, 100)?;
    let workspace = tempfile::tempdir()?;
    let processes = Arc::new(ProcessTools::new(WorkspaceTools::new(workspace.path())?));
    processes.set_wake(fixture.wake.clone());
    processes.set_store(fixture.store.clone());
    let mut tools = ToolRegistry::new();
    processes.register(&mut tools)?;
    fixture.agents.register(&mut tools)?;
    fixture.root = Arc::new(Harness::new(
        fixture.store.clone(),
        fixture.probe.clone(),
        tools.clone(),
        HarnessConfig::default(),
    ));
    let _registration = fixture.wake.register_harness(fixture.root.clone());
    let _driver = fixture.wake.start_driver();
    let receipt = tools
        .get("exec")
        .unwrap()
        .invoke(
            fixture.context(&fixture.parent),
            json!({"command":gated_completion_command(),"background":true,
                "timeoutMs":exec_test_timeout().as_millis()}),
        )
        .await?;
    let receipt: Value = serde_json::from_str(&receipt.content)?;
    eventually(|| workspace.path().join("ready").exists()).await;
    let child = fixture.start("gate")?;
    assert_eq!(
        fixture.entered().await,
        child["sessionId"].as_str().unwrap()
    );
    assert!(fixture.store.runs(&fixture.parent)?.is_empty());
    // Shell startup is outside the batching window. Complete the ready exec,
    // then release the ready child as soon as its completion is observable.
    release_completion(&workspace.path().join("release")).await?;
    assert_completed_exec(&fixture, &tools, &receipt["handle"]).await?;
    fixture.probe.release.notify_waiters();
    eventually(|| fixture.notices(&fixture.parent).len() == 1).await;
    fixture.root.wait_for_idle(&fixture.parent).await?;
    let notices = fixture.notices(&fixture.parent);
    assert!(notices[0].contains(child["sessionId"].as_str().unwrap()));
    assert!(notices[0].contains(receipt["handle"].as_str().unwrap()));
    assert!(notices[0].contains("exited (exit code 0); stdout 9 bytes, stderr 0 bytes"));
    assert_eq!(fixture.store.runs(&fixture.parent)?.len(), 1);
    assert!(fixture.agents.take_reminders(&fixture.parent).is_empty());
    assert!(processes.take_reminders(&fixture.parent).is_empty());
    processes.shutdown().await;
    Ok(())
}
#[tokio::test]
async fn timed_out_child_wakes_parent_with_terminal_timeout_status() -> Result<()> {
    let fixture = Fixture::new(true, 10)?;
    let _registration = fixture.wake.register_harness(fixture.root.clone());
    let _driver = fixture.wake.start_driver();
    fixture.agents.start(
        &fixture.context(&fixture.parent),
        json!({"task":"gate","timeout":20}),
    )?;
    eventually(|| fixture.notices(&fixture.parent).len() == 1).await;
    assert!(fixture.notices(&fixture.parent)[0].contains("status timed_out"));
    Ok(())
}
#[tokio::test]
async fn streaming_timeout_keeps_partial_result_out_of_durable_parent_notice() -> Result<()> {
    let mut fixture = Fixture::new(true, 10)?;
    let _registration = fixture.wake.register_harness(fixture.root.clone());
    let _driver = fixture.wake.start_driver();
    let child = fixture.agents.start(
        &fixture.context(&fixture.parent),
        json!({"task":"stream-timeout","timeout":200}),
    )?;
    assert_eq!(
        fixture.entered().await,
        child["sessionId"].as_str().unwrap()
    );
    eventually(|| fixture.notices(&fixture.parent).len() == 1).await;
    fixture.root.wait_for_idle(&fixture.parent).await?;
    let notices = fixture.notices(&fixture.parent);
    assert!(notices[0].contains("status timed_out"));
    assert!(!notices[0].contains("transient unfinished child fragment"));
    let live = fixture
        .agents
        .wait(
            &fixture.parent,
            child["sessionId"].as_str().unwrap(),
            Some(3000),
        )
        .await?;
    assert_eq!(live["status"], "timed_out");
    assert_eq!(live["text"], "transient unfinished child fragment");
    assert!(
        fixture
            .store
            .messages(child["sessionId"].as_str().unwrap())?
            .iter()
            .all(|message| !message
                .content
                .contains("transient unfinished child fragment"))
    );
    Ok(())
}
#[tokio::test]
async fn tool_budget_final_child_delivers_ordinary_terminal_completion() -> Result<()> {
    let fixture = Fixture::new(true, 10)?;
    let _registration = fixture.wake.register_harness(fixture.root.clone());
    let _driver = fixture.wake.start_driver();
    let child = fixture.agents.start(
        &fixture.context(&fixture.parent),
        json!({"task":"budget-final","max_tool_calls":0}),
    )?;
    eventually(|| fixture.notices(&fixture.parent).len() == 1).await;
    assert!(fixture.notices(&fixture.parent)[0].contains("status idle"));
    let child = child["sessionId"].as_str().unwrap();
    let requests = fixture.probe.requests.lock().unwrap().clone();
    let round = requests.iter().find(|r| r.session_id == child).unwrap();
    // P3: the tools-off round keeps its declarations and carries a tools_off reminder.
    let reminded = format!("{:?}", round.messages).contains("tools_off:");
    assert!(!round.tools.is_empty() && reminded);
    let stored = fixture.store.messages(child)?;
    assert!(stored.iter().all(|m| m.role != Role::Tool));
    Ok(())
}
#[tokio::test]
async fn completion_wake_chains_have_no_fixed_turn_cap() -> Result<()> {
    let fixture = Fixture::new(true, 5)?;
    let _registration = fixture.wake.register_harness(fixture.root.clone());
    let _driver = fixture.wake.start_driver();
    for index in 0..5 {
        fixture.start(&format!("child-{index}"))?;
        eventually(|| fixture.notices(&fixture.parent).len() == index + 1).await;
        fixture.root.wait_for_idle(&fixture.parent).await?;
    }
    assert_eq!(fixture.store.runs(&fixture.parent)?.len(), 5);
    Ok(())
}
#[tokio::test]
async fn evicted_visible_parent_recovers_its_normal_wake_lifecycle() -> Result<()> {
    let mut fixture = Fixture::limited(true, 20, Some(2))?;
    let _registration = fixture.wake.register_harness(fixture.root.clone());
    let _driver = fixture.wake.start_driver();
    let parent = fixture.start("nested")?;
    let leaf_id = fixture.entered().await;
    let owner = parent["sessionId"].as_str().unwrap();
    eventually(|| {
        fixture.agents.status(&fixture.parent, &json!({})).unwrap()["children"]
            .as_array()
            .unwrap()
            .iter()
            .any(|child| child["sessionId"] == owner && child["status"] == "idle")
    })
    .await;
    fixture.start("replacement")?;
    assert!(
        fixture.agents.status(&fixture.parent, &json!({}))?["children"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["sessionId"] == owner && entry["handle"].is_null())
    );
    fixture.probe.release.notify_waiters();
    eventually(|| fixture.store.runs(owner).unwrap().len() == 2).await;
    assert_eq!(fixture.notices(owner).len(), 1);
    assert!(fixture.notices(owner)[0].contains(&leaf_id));
    Ok(())
}
#[tokio::test]
async fn unfinished_exec_is_not_replayed_or_claimed_complete_on_recovery() -> Result<()> {
    let fixture = Fixture::new(true, 10)?;
    let workspace = tempfile::tempdir()?;
    let processes = Arc::new(ProcessTools::new(WorkspaceTools::new(workspace.path())?));
    processes.set_store(fixture.store.clone());
    let mut tools = ToolRegistry::new();
    processes.register(&mut tools)?;
    tools
        .get("exec")
        .unwrap()
        .invoke(
            fixture.context(&fixture.parent),
            json!({"command":"sleep 5","background":true}),
        )
        .await?;
    let restored = Arc::new(ProcessTools::new(WorkspaceTools::new(workspace.path())?));
    restored.set_store(fixture.store.clone());
    assert!(restored.take_reminders(&fixture.parent).is_empty());
    assert!(
        restored.jobs(&fixture.parent)?["jobs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    processes.shutdown().await;
    Ok(())
}
#[tokio::test]
async fn host_drop_releases_harness_and_agent_registry() -> Result<()> {
    let fixture = Fixture::new(true, 10)?;
    let harness = Arc::downgrade(&fixture.root);
    let agents = Arc::downgrade(&fixture.agents);
    let coordinator = Arc::downgrade(&fixture.wake);
    let registration = fixture.wake.register_harness(fixture.root.clone());
    let driver = fixture.wake.start_driver();
    fixture.start("child")?;
    eventually(|| fixture.notices(&fixture.parent).len() == 1).await;
    fixture.root.wait_for_idle(&fixture.parent).await?;
    drop(registration);
    drop(driver);
    drop(fixture);
    eventually(|| {
        harness.upgrade().is_none() && agents.upgrade().is_none() && coordinator.upgrade().is_none()
    })
    .await;
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_steering_and_exec_completion_do_not_deadlock_or_duplicate() -> Result<()> {
    for _ in 0..12 {
        let mut fixture = Fixture::new(true, 0)?;
        let workspace = tempfile::tempdir()?;
        let processes = Arc::new(ProcessTools::new(WorkspaceTools::new(workspace.path())?));
        processes.set_wake(fixture.wake.clone());
        processes.set_store(fixture.store.clone());
        let mut tools = ToolRegistry::new();
        processes.register(&mut tools)?;
        fixture.root = Arc::new(Harness::new(
            fixture.store.clone(),
            fixture.probe.clone(),
            tools.clone(),
            HarnessConfig::default(),
        ));
        let _registration = fixture.wake.register_harness(fixture.root.clone());
        let _driver = fixture.wake.start_driver();
        fixture
            .root
            .start(&fixture.parent, "gate", RunOptions::default())?;
        fixture.entered().await;
        let receipt = tools
            .get("exec")
            .unwrap()
            .invoke(
                fixture.context(&fixture.parent),
                json!({"command":gated_completion_command(),"background":true,
                    "timeoutMs":exec_test_timeout().as_millis()}),
            )
            .await?;
        let receipt: Value = serde_json::from_str(&receipt.content)?;
        eventually(|| workspace.path().join("ready").exists()).await;
        let root = fixture.root.clone();
        let parent = fixture.parent.clone();
        let process_tools = processes.clone();
        let release = workspace.path().join("release");
        let steer = tokio::spawn(async move {
            assert_eq!(process_tools.jobs(&parent)?["jobs"][0]["status"], "running");
            root.steer(&parent, "steered")?;
            release_completion(&release).await?;
            // Shell startup is finished before steering; the remaining inputs
            // race the released process and the concurrent completion poll.
            for _ in 1..20 {
                root.steer(&parent, "steered")?;
                tokio::task::yield_now().await;
            }
            Ok::<_, anyhow::Error>(())
        });
        let steering = async {
            let timeout = Duration::from_secs(if cfg!(windows) { 15 } else { 2 });
            tokio::time::timeout(timeout, steer).await???;
            Ok::<_, anyhow::Error>(())
        };
        tokio::try_join!(
            steering,
            assert_completed_exec(&fixture, &tools, &receipt["handle"]),
        )?;
        fixture.probe.release.notify_waiters();
        eventually(|| fixture.notices(&fixture.parent).len() == 1).await;
        fixture.root.wait_for_idle(&fixture.parent).await?;
        assert_eq!(fixture.notices(&fixture.parent).len(), 1);
        assert!(fixture.notices(&fixture.parent)[0].contains(receipt["handle"].as_str().unwrap()));
        assert!(
            fixture.notices(&fixture.parent)[0]
                .contains("exited (exit code 0); stdout 9 bytes, stderr 0 bytes")
        );
        assert!(processes.take_reminders(&fixture.parent).is_empty());
        processes.shutdown().await;
    }
    Ok(())
}
#[tokio::test]
async fn child_pending_completion_recovers_on_next_turn_without_restart_wake() -> Result<()> {
    let fixture = Fixture::new(true, 20)?;
    let child = fixture.start("child")?;
    eventually(|| {
        fixture.agents.status(&fixture.parent, &json!({})).unwrap()["children"][0]["status"]
            == "idle"
    })
    .await;
    let restored = Arc::new(AgentTools::new(
        fixture.store.clone(),
        Harness::new(
            fixture.store.clone(),
            fixture.probe.clone(),
            ToolRegistry::new(),
            HarnessConfig::default(),
        ),
        None,
    ));
    let mut tools = ToolRegistry::new();
    restored.register(&mut tools)?;
    let root = Arc::new(Harness::new(
        fixture.store.clone(),
        fixture.probe.clone(),
        tools,
        HarnessConfig::default(),
    ));
    let wake = restored.wake_coordinator();
    wake.configure(true, Duration::from_millis(10));
    let _registration = wake.register_harness(root.clone());
    let _driver = wake.start_driver();
    wake_barrier(&fixture.store, &wake).await?;
    assert!(fixture.store.runs(&fixture.parent)?.is_empty());
    root.prompt(&fixture.parent, "typed", RunOptions::default())
        .await?;
    assert_eq!(fixture.notices(&fixture.parent).len(), 1);
    assert!(fixture.notices(&fixture.parent)[0].contains(child["sessionId"].as_str().unwrap()));
    root.prompt(&fixture.parent, "next", RunOptions::default())
        .await?;
    assert_eq!(fixture.notices(&fixture.parent).len(), 1);
    Ok(())
}
#[tokio::test]
async fn known_exec_completion_recovers_next_turn_without_proactive_restart_wake() -> Result<()> {
    let fixture = Fixture::new(true, 10)?;
    let workspace = tempfile::tempdir()?;
    let processes = Arc::new(ProcessTools::new(WorkspaceTools::new(workspace.path())?));
    processes.set_store(fixture.store.clone());
    let mut tools = ToolRegistry::new();
    processes.register(&mut tools)?;
    processes.register_wait(&mut tools)?;
    let receipt = tools
        .get("exec")
        .unwrap()
        .invoke(
            fixture.context(&fixture.parent),
            json!({"command":completed_command(),"background":true}),
        )
        .await?;
    let handle: Value = serde_json::from_str(&receipt.content)?;
    let result = tools
        .get("wait")
        .unwrap()
        .invoke(
            fixture.context(&fixture.parent),
            json!({"handle":handle["handle"],
                "timeoutMs":if cfg!(windows) { 15_000 } else { 3_000 }}),
        )
        .await?;
    let result: Value = serde_json::from_str(&result.content)?;
    assert_eq!(result["status"], "exited", "{result}");
    assert_eq!(result["exitCode"], 0, "{result}");
    assert_completed_exec(&fixture, &tools, &handle["handle"]).await?;
    let restored = Arc::new(ProcessTools::new(WorkspaceTools::new(workspace.path())?));
    restored.set_store(fixture.store.clone());
    let mut tools = ToolRegistry::new();
    restored.register(&mut tools)?;
    let root = Arc::new(Harness::new(
        fixture.store.clone(),
        fixture.probe.clone(),
        tools,
        HarnessConfig::default(),
    ));
    let wake = restored.wake_coordinator();
    wake.configure(true, Duration::from_millis(10));
    let _registration = wake.register_harness(root.clone());
    let _driver = wake.start_driver();
    wake_barrier(&fixture.store, &wake).await?;
    assert!(fixture.store.runs(&fixture.parent)?.is_empty());
    assert!(
        restored.jobs(&fixture.parent)?["jobs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    root.prompt(&fixture.parent, "typed", RunOptions::default())
        .await?;
    assert_eq!(fixture.notices(&fixture.parent).len(), 1);
    assert!(fixture.notices(&fixture.parent)[0].contains(handle["handle"].as_str().unwrap()));
    assert!(
        fixture.notices(&fixture.parent)[0]
            .contains("exited (exit code 0); stdout 9 bytes, stderr 0 bytes")
    );
    root.prompt(&fixture.parent, "next", RunOptions::default())
        .await?;
    assert_eq!(fixture.notices(&fixture.parent).len(), 1);
    processes.shutdown().await;
    Ok(())
}
