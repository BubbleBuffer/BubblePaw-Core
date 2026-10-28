//! Scripted child API acceptance. No live providers or external web access.
use anyhow::{Result, bail};
use async_trait::async_trait;
use bubblepaw_core::{
    ApprovalMode, ApprovalPolicy, CancellationToken, Harness, HarnessConfig, ModelRequest,
    ModelResponse, Provider, Role, RunOptions, Store, TextCallback, ToolCall, ToolContext,
    ToolRegistry,
};
use bubblepaw_runner::{
    agent_handles::register_agent_handles_weak,
    agents::{AgentProfile, AgentRegistry, AgentTools},
    artifacts::ArtifactStore,
    codemode::CodeMode,
    config::Agents,
    tools::{ProcessTools, register_native_tools},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, mpsc};

enum Step {
    Text(String),
    StreamSubmit(String, Value),
    Js(String, String),
    Hang,
    StreamThenHang(String),
    Fail,
}
struct Probe {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<ModelRequest>>,
    entered: mpsc::UnboundedSender<()>,
    dropped: Arc<AtomicUsize>,
    release: Notify,
}
struct Inflight(Arc<AtomicUsize>);
impl Drop for Inflight {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[async_trait]
impl Provider for Probe {
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("script exhausted");
        match step {
            Step::Text(text) => Ok(ModelResponse {
                text,
                ..Default::default()
            }),
            Step::Js(text, code) => Ok(ModelResponse {
                text,
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments: json!({"code":code}),
                }],
                ..Default::default()
            }),
            Step::Hang => {
                let _inflight = Inflight(self.dropped.clone());
                self.entered.send(())?;
                self.release.notified().await;
                Ok(ModelResponse {
                    text: "released answer".into(),
                    ..Default::default()
                })
            }
            Step::StreamSubmit(text, value) => {
                on_text(&text);
                Ok(ModelResponse {
                    text,
                    tool_calls: vec![ToolCall {
                        id: uuid::Uuid::new_v4().to_string(),
                        name: "js".into(),
                        arguments: json!({"code":format!("return await tools.submit_result({value});")}),
                    }],
                    ..Default::default()
                })
            }
            Step::StreamThenHang(text) => {
                let _inflight = Inflight(self.dropped.clone());
                on_text(&text);
                self.entered.send(())?;
                self.release.notified().await;
                Ok(ModelResponse {
                    text,
                    ..Default::default()
                })
            }
            Step::Fail => bail!("scripted failure"),
        }
    }
}
struct Fixture {
    _directory: tempfile::TempDir,
    store: Store,
    manager: Arc<AgentTools>,
    owner: ToolContext,
    probe: Arc<Probe>,
    entered: mpsc::UnboundedReceiver<()>,
    repl: Arc<CodeMode>,
    processes: Arc<ProcessTools>,
}
impl Fixture {
    fn new(steps: Vec<Step>) -> Result<Self> {
        Self::with_limits(steps, Agents::default())
    }
    fn with_limits(steps: Vec<Step>, limits: Agents) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let store = Store::open_data_dir(directory.path())?;
        let parent = store.create_session("parent")?;
        let (entered_tx, entered) = mpsc::unbounded_channel();
        let probe = Arc::new(Probe {
            steps: Mutex::new(steps.into()),
            requests: Mutex::default(),
            entered: entered_tx,
            dropped: Arc::new(AtomicUsize::new(0)),
            release: Notify::new(),
        });
        let mut tools = ToolRegistry::new();
        let processes = register_native_tools(&mut tools, directory.path(), false)?;
        let repl = Arc::new(CodeMode::new(
            directory.path().to_owned(),
            Arc::new(ArtifactStore::new(
                directory.path(),
                directory.path().join("artifacts"),
            )?),
        )?);
        repl.register(&mut tools)?;
        let harness = Harness::new(
            store.clone(),
            probe.clone(),
            tools,
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                ..Default::default()
            },
        );
        let manager = Arc::new(
            AgentTools::new(store.clone(), harness, None)
                .with_limits(limits)
                .with_repl(repl.clone())
                .with_processes(processes.clone()),
        );
        let owner = ToolContext {
            session_id: parent.id,
            run_id: "parent-run".into(),
            invocation_id: "child-launch".into(),
            cancellation: CancellationToken::new(),
            approval: ApprovalPolicy {
                confirm: ApprovalMode::Allow,
                ..Default::default()
            },
        };
        Ok(Self {
            _directory: directory,
            store,
            manager,
            owner,
            probe,
            entered,
            repl,
            processes,
        })
    }
    async fn wait(&self, child: &Value) -> Result<Value> {
        let result = self
            .manager
            .wait(
                &self.owner.session_id,
                child["sessionId"].as_str().unwrap(),
                Some(5000),
            )
            .await?;
        assert_ne!(result["status"], "timeout", "{result}");
        Ok(result)
    }
    async fn entered(&mut self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(3), self.entered.recv())
            .await?
            .expect("provider entry signal");
        Ok(())
    }
    async fn shutdown(&self) -> Result<()> {
        self.manager.shutdown().await?;
        self.repl.shutdown().await;
        self.processes.shutdown().await;
        Ok(())
    }
}
#[tokio::test]
async fn timeout_drops_inflight_provider_and_preserves_reusable_history() -> Result<()> {
    let mut fixture = Fixture::new(vec![Step::Hang, Step::Hang])?;
    let child = fixture
        .manager
        .start(&fixture.owner, json!({"task":"first task","timeout":200}))?;
    fixture.entered().await?;
    let timed = fixture.wait(&child).await?;
    assert_eq!(timed["status"], "timed_out", "{timed}");
    assert_eq!(fixture.probe.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture.store.runs(child["sessionId"].as_str().unwrap())?[0].status,
        "cancelled"
    );
    let restored = AgentTools::from_profiles(
        fixture.store.clone(),
        vec![fixture.manager.profile("read-only").unwrap().clone()],
    )?;
    assert_eq!(
        restored.status(
            &fixture.owner.session_id,
            &json!({"sessionId":child["sessionId"]})
        )?["status"],
        "timed_out"
    );
    fixture.manager.send(
        &fixture.owner,
        json!({"sessionId":child["sessionId"],"text":"continue task"}),
    )?;
    fixture.entered().await?;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        fixture.manager.status(
            &fixture.owner.session_id,
            &json!({"sessionId":child["sessionId"]})
        )?["status"],
        "running",
        "omitted follow-up timeout must not reuse old deadline"
    );
    fixture.probe.release.notify_one();
    assert_eq!(fixture.wait(&child).await?["status"], "completed");
    {
        let requests = fixture.probe.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|m| m.role == Role::User && m.content == "first task")
        );
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|m| m.role == Role::User && m.content == "continue task")
        );
    }
    fixture.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn timeout_returns_current_unfinished_stream_without_archiving_partial_message() -> Result<()>
{
    let mut fixture = Fixture::new(vec![
        Step::StreamThenHang("unfinished streamed answer".into()),
        Step::Text("continued".into()),
    ])?;
    let child = fixture
        .manager
        .start(&fixture.owner, json!({"task":"stream","timeout":200}))?;
    fixture.entered().await?;
    let result = fixture.wait(&child).await?;
    assert_eq!(result["status"], "timed_out", "{result}");
    assert_eq!(result["text"], "unfinished streamed answer");
    assert_eq!(fixture.probe.dropped.load(Ordering::SeqCst), 1);
    assert!(
        !fixture
            .store
            .messages(child["sessionId"].as_str().unwrap())?
            .iter()
            .any(|m| m.role == Role::Assistant && m.content == "unfinished streamed answer")
    );
    let session = child["sessionId"].as_str().unwrap();
    let run = fixture.store.runs(session)?.into_iter().last().unwrap();
    let (metadata, bytes) = fixture
        .store
        .attachment(session, &format!("bubblepaw-agent-failure-v1:{}", run.id))?
        .expect("timeout metadata");
    assert!(!metadata.contains("unfinished streamed answer"));
    assert!(bytes.is_empty());
    let reattached = AgentTools::from_profiles(
        fixture.store.clone(),
        vec![fixture.manager.profile("read-only").unwrap().clone()],
    )?;
    assert_eq!(
        reattached.status(&fixture.owner.session_id, &json!({"sessionId":session}))?["text"],
        "",
        "unfinished stream expires with the live manager"
    );
    fixture.manager.send(
        &fixture.owner,
        json!({"sessionId":child["sessionId"],"text":"continue"}),
    )?;
    assert_eq!(fixture.wait(&child).await?["text"], "continued");
    fixture.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn timeout_cleans_child_commands_and_js_and_returns_last_partial_answer() -> Result<()> {
    #[cfg(windows)]
    let command = "ping -n 31 127.0.0.1 >nul";
    #[cfg(not(windows))]
    let command = "sleep 30";
    let code = format!(
        "const p=await tools.exec({}); text(p.handle);",
        json!({"command":command,"background":true})
    );
    let mut fixture = Fixture::new(vec![
        Step::Js("last partial answer".into(), code),
        Step::Hang,
        Step::Text("continued".into()),
    ])?;
    let child = fixture.manager.start(
        &fixture.owner,
        json!({"task":"work then block","timeout":1500}),
    )?;
    fixture.entered().await?;
    let session = child["sessionId"].as_str().unwrap();
    assert!(
        fixture.processes.continuation_status(session)["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["status"] == "running")
    );
    let timed = fixture.wait(&child).await?;
    assert_eq!(timed["status"], "timed_out", "{timed}");
    assert_eq!(timed["text"], "last partial answer");
    assert!(
        fixture.processes.continuation_status(session)["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["status"] != "running")
    );
    assert!(
        fixture.repl.status(session)["environments"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    fixture.manager.send(
        &fixture.owner,
        json!({"sessionId":session,"text":"resume after timeout","timeout":1000}),
    )?;
    assert_eq!(fixture.wait(&child).await?["text"], "continued");
    assert!(fixture.store.pending_effects(session)?.is_empty());
    fixture.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn all_terminal_outcomes_allow_new_input_and_bad_controls_reject_before_spawn() -> Result<()>
{
    let fixture = Fixture::new(vec![
        Step::Fail,
        Step::Text("after failure".into()),
        Step::Text("after cancel".into()),
    ])?;
    for input in [
        json!({"task":"x","max_tool_calls":-1}),
        json!({"task":"x","max_tool_calls":1.5}),
        json!({"task":"x","timeout":-1}),
        json!({"task":"x","timeout":"100"}),
        json!({"task":"x","output_schema":{"name":"bad","schema":{"type":"object"}}}),
    ] {
        assert!(fixture.manager.start(&fixture.owner, input).is_err());
    }
    assert_eq!(fixture.store.list_sessions()?.len(), 1);
    let child = fixture
        .manager
        .start(&fixture.owner, json!({"task":"fails"}))?;
    assert_eq!(fixture.wait(&child).await?["status"], "failed");
    fixture.manager.send(
        &fixture.owner,
        json!({"sessionId":child["sessionId"],"text":"follow up"}),
    )?;
    assert_eq!(fixture.wait(&child).await?["text"], "after failure");
    assert_eq!(
        fixture
            .manager
            .cancel(
                &fixture.owner.session_id,
                child["sessionId"].as_str().unwrap()
            )
            .await?["status"],
        "cancelled"
    );
    fixture.manager.send(
        &fixture.owner,
        json!({"sessionId":child["sessionId"],"text":"continue manually cancelled"}),
    )?;
    assert_eq!(fixture.wait(&child).await?["text"], "after cancel");
    assert_eq!(
        fixture
            .store
            .runs(child["sessionId"].as_str().unwrap())?
            .len(),
        3
    );
    fixture.shutdown().await?;
    Ok(())
}

#[derive(Default)]
struct ParentProbe(Mutex<Vec<ModelRequest>>);
#[async_trait]
impl Provider for ParentProbe {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.0.lock().unwrap().push(request);
        Ok(ModelResponse {
            text: "parent answer".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn configured_short_reminders_reach_parent_once_per_interval_for_untouched_children()
-> Result<()> {
    let mut fixture = Fixture::with_limits(
        vec![Step::Hang, Step::Hang, Step::Text("finished answer".into())],
        Agents {
            stale_reminder_secs: 1,
            ..Default::default()
        },
    )?;
    let untouched = fixture
        .manager
        .start(&fixture.owner, json!({"task":"untouched"}))?;
    fixture.entered().await?;
    let touched = fixture
        .manager
        .start(&fixture.owner, json!({"task":"touched"}))?;
    fixture.entered().await?;
    let finished = fixture
        .manager
        .start(&fixture.owner, json!({"task":"finished"}))?;
    assert_eq!(fixture.wait(&finished).await?["status"], "completed");
    let provider = Arc::new(ParentProbe::default());
    let mut tools = ToolRegistry::new();
    fixture.manager.register(&mut tools)?;
    let parent = Harness::new(
        fixture.store.clone(),
        provider.clone(),
        tools,
        HarnessConfig::default(),
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    fixture.manager.status(
        &fixture.owner.session_id,
        &json!({"sessionId":touched["sessionId"]}),
    )?;
    parent
        .prompt(
            &fixture.owner.session_id,
            "first poll",
            RunOptions::default(),
        )
        .await?;
    parent
        .prompt(
            &fixture.owner.session_id,
            "immediate second poll",
            RunOptions::default(),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    fixture.manager.send(
        &fixture.owner,
        json!({"sessionId":touched["sessionId"],"text":"fresh guidance"}),
    )?;
    parent
        .prompt(
            &fixture.owner.session_id,
            "next interval",
            RunOptions::default(),
        )
        .await?;
    {
        let requests = provider.0.lock().unwrap();
        assert_eq!(requests.len(), 3);
        for (index, request) in requests.iter().enumerate() {
            let rendered = serde_json::to_string(request)?;
            // Prior host notices remain history; only the interval adds a new one.
            assert_eq!(
                rendered
                    .matches(untouched["handle"].as_str().unwrap())
                    .count(),
                1 + usize::from(index == 2),
                "{rendered}"
            );
            assert!(
                !rendered.contains(touched["handle"].as_str().unwrap()),
                "{rendered}"
            );
            assert!(
                !rendered.contains(finished["handle"].as_str().unwrap()),
                "{rendered}"
            );
        }
    }
    fixture.shutdown().await?;
    Ok(())
}

struct NestedProbe {
    entered: mpsc::UnboundedSender<String>,
    dropped: Arc<AtomicUsize>,
}
#[async_trait]
impl Provider for NestedProbe {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let user = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .unwrap();
        if user.content == "hang grandchild" {
            let _inflight = Inflight(self.dropped.clone());
            self.entered.send(request.session_id)?;
            return std::future::pending().await;
        }
        Ok(ModelResponse {
            tool_calls: vec![ToolCall {
                id: uuid::Uuid::new_v4().to_string(),
                name: "js".into(),
                arguments: json!({"code":"const child=await tools.agent({task:'hang grandchild'}); const result=await wait({handle:child.handle}); text(result);"}),
            }],
            ..Default::default()
        })
    }
}

#[path = "child_controls/timeout_cleanup.rs"]
mod timeout_cleanup;

#[path = "child_controls/structured.rs"]
mod structured;
