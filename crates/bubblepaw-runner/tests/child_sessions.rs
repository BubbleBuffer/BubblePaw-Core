//! PROBE/SCRIPTED acceptance only. No provider transport or live model calls.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    Approval, ApprovalMode, ApprovalPolicy, CancellationToken, Decision, EventKind, Harness,
    HarnessConfig, HarnessEvent, Message, ModelRequest, ModelResponse, Provider, Role, RunOptions,
    Store, TextCallback, Tool, ToolCall, ToolContext, ToolDefinition, ToolDispatcher, ToolRegistry,
    ToolResult,
};
use bubblepaw_runner::{
    agent_handles::register_agent_handles,
    agents::{AgentProfile, AgentRegistry, AgentTools},
    artifacts::ArtifactStore,
    codemode::CodeMode,
    config::Agents,
    profiles::child_agent_host,
    tools::register_native_tools,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, broadcast, mpsc};

const DEADLINE: Duration = Duration::from_secs(5);

struct Probe {
    requests: Mutex<Vec<ModelRequest>>,
    entered: mpsc::UnboundedSender<String>,
    release: Notify,
}
#[async_trait]
impl Provider for Probe {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let (index, user) = request
            .messages
            .iter()
            .enumerate()
            .rev()
            .find(|(_, message)| message.role == Role::User)
            .unwrap();
        let input = user.content.clone();
        let has_result = request.messages[index + 1..]
            .iter()
            .any(|message| message.role == Role::Tool);
        let session = request.session_id.clone();
        self.requests.lock().unwrap().push(request);
        if input == "fail" {
            anyhow::bail!("synthetic provider failure");
        }
        if input == "block" || input == "gate" {
            self.entered.send(session)?;
            tokio::select! {
                _ = cancellation.cancelled() => anyhow::bail!("synthetic provider cancelled"),
                _ = self.release.notified(), if input == "gate" => {},
            }
        }
        if input == "effect" && !has_result {
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "effect".into(),
                    arguments: json!({}),
                }],
                ..Default::default()
            });
        }
        if let Some(arguments) = input.strip_prefix("CALL:")
            && !has_result
        {
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "bridge".into(),
                    arguments: serde_json::from_str(arguments)?,
                }],
                ..Default::default()
            });
        }
        Ok(ModelResponse {
            text: format!("answer:{input}"),
            ..Default::default()
        })
    }
}

struct Bridge;
#[async_trait]
impl Tool for Bridge {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bridge".into(),
            description: "scripted host call".into(),
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
        let result = dispatcher
            .invoke(
                context,
                input["target"].as_str().unwrap(),
                input["input"].clone(),
            )
            .await?;
        Ok(ToolResult {
            pending: false,
            ..result
        })
    }
}

struct CountedEffect {
    calls: Arc<AtomicUsize>,
    policies: Arc<Mutex<Vec<ApprovalPolicy>>>,
}
#[async_trait]
impl Tool for CountedEffect {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "effect".into(),
            description: "counted synthetic external effect".into(),
            input_schema: json!({"type":"object"}),
            read_only: false,
            approval: Approval::Confirm,
        }
    }
    async fn invoke(&self, context: ToolContext, _: Value) -> Result<ToolResult> {
        self.policies.lock().unwrap().push(context.approval);
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text("effect completed"))
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    store: Store,
    parent: Harness,
    manager: Arc<AgentTools>,
    owner: ToolContext,
    probe: Arc<Probe>,
    entered: mpsc::UnboundedReceiver<String>,
    effects: Arc<AtomicUsize>,
    policies: Arc<Mutex<Vec<ApprovalPolicy>>>,
}
impl Fixture {
    fn new(
        limits: Agents,
        parent_policy: ApprovalPolicy,
        profile_policy: Option<ApprovalPolicy>,
    ) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let store = Store::open_data_dir(directory.path())?;
        let session = store.create_session("parent")?;
        let (entered_tx, entered) = mpsc::unbounded_channel();
        let probe = Arc::new(Probe {
            requests: Mutex::default(),
            entered: entered_tx,
            release: Notify::new(),
        });
        let effects = Arc::new(AtomicUsize::new(0));
        let policies = Arc::new(Mutex::new(Vec::new()));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(CountedEffect {
            calls: effects.clone(),
            policies: policies.clone(),
        }))?;
        let child = Arc::new(Harness::new(
            store.clone(),
            probe.clone(),
            tools,
            HarnessConfig {
                approval: allow(),
                ..Default::default()
            },
        ));
        let manager = Arc::new(
            AgentTools::from_profiles(
                store.clone(),
                vec![AgentProfile {
                    name: "read-only".into(),
                    description: "synthetic child".into(),
                    harness: child,
                    model: None,
                    max_rounds: Some(12),
                    approval: profile_policy,
                }],
            )?
            .with_limits(limits),
        );
        let mut parent_tools = ToolRegistry::new();
        parent_tools.register(Arc::new(Bridge))?;
        manager.register(&mut parent_tools)?;
        register_agent_handles(&mut parent_tools, manager.clone())?;
        let parent = Harness::new(
            store.clone(),
            probe.clone(),
            parent_tools,
            HarnessConfig {
                approval: parent_policy.clone(),
                ..Default::default()
            },
        );
        let owner = ToolContext {
            session_id: session.id,
            run_id: "parent-probe-run".into(),
            invocation_id: "probe-call".into(),
            cancellation: CancellationToken::new(),
            approval: parent_policy,
        };
        Ok(Self {
            _directory: directory,
            store,
            parent,
            manager,
            owner,
            probe,
            entered,
            effects,
            policies,
        })
    }
    fn spawn(&self, task: &str) -> Result<Value> {
        self.manager.start(&self.owner, json!({"task":task}))
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
        assert_ne!(
            result["status"], "timeout",
            "child failed to settle: {result}"
        );
        Ok(result)
    }
    async fn entered(&mut self, child: &Value) -> Result<()> {
        assert_eq!(
            tokio::time::timeout(DEADLINE, self.entered.recv())
                .await?
                .unwrap(),
            child["sessionId"]
        );
        Ok(())
    }
    async fn call(&self, target: &str, input: Value, options: RunOptions) -> Result<Value> {
        let run = tokio::time::timeout(
            DEADLINE,
            self.parent.prompt(
                &self.owner.session_id,
                format!("CALL:{}", json!({"target":target,"input":input})),
                options,
            ),
        )
        .await??;
        assert_eq!(run.status, "completed");
        let messages = self.store.messages(&self.owner.session_id)?;
        let reply = messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Tool)
            .unwrap();
        assert!(!reply.is_error, "{}", reply.content);
        Ok(serde_json::from_str(&reply.content)?)
    }
}
fn allow() -> ApprovalPolicy {
    ApprovalPolicy {
        confirm: ApprovalMode::Allow,
        ..Default::default()
    }
}
async fn event_matching(
    events: &mut broadcast::Receiver<HarnessEvent>,
    predicate: impl Fn(&HarnessEvent) -> bool,
) -> Result<HarnessEvent> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let event = events.recv().await?;
            if predicate(&event) {
                return Ok(event);
            }
        }
    })
    .await?
}

#[tokio::test]
async fn idle_child_accepts_new_turn_and_status_listing_preserves_unread() -> Result<()> {
    let fixture = Fixture::new(Agents::default(), allow(), None)?;
    let mut events = fixture.parent.subscribe();
    let child = fixture.spawn("first")?;
    event_matching(&mut events, |event| {
        event.session_id == child["sessionId"]
            && matches!(event.kind, EventKind::RunFinished { .. })
    })
    .await?;
    // RunFinished precedes host cleanup. Wait for the observable idle state,
    // bounded and without consuming the result through a specific status.
    let listed = tokio::time::timeout(DEADLINE, async {
        loop {
            let list = fixture
                .manager
                .status(&fixture.owner.session_id, &json!({}))?;
            if list["children"][0]["status"] == "idle" {
                return Ok::<Value, anyhow::Error>(list);
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    assert_eq!(listed["children"][0]["unread"], true);
    assert_eq!(
        fixture
            .manager
            .status(&fixture.owner.session_id, &json!({}))?["children"][0]["unread"],
        true
    );
    let result = fixture.wait(&child).await?;
    assert_eq!(result["status"], "completed");
    assert_eq!(result["text"], "answer:first");
    assert_eq!(
        fixture
            .manager
            .status(&fixture.owner.session_id, &json!({}))?["children"][0]["unread"],
        false
    );
    let sent = fixture.manager.send(
        &fixture.owner,
        json!({"sessionId":child["sessionId"],"text":"second"}),
    )?;
    assert_eq!(sent["status"], "running");
    let second = fixture.wait(&child).await?;
    assert_eq!(second["status"], "completed");
    assert_eq!(second["text"], "answer:second");
    let history = fixture
        .store
        .messages(child["sessionId"].as_str().unwrap())?;
    assert_eq!(
        history
            .iter()
            .filter(|message| message.role == Role::User)
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert_eq!(
        fixture
            .store
            .runs(child["sessionId"].as_str().unwrap())?
            .len(),
        2
    );
    let third = fixture.spawn("status consumption")?;
    event_matching(&mut events, |event| {
        event.session_id == third["sessionId"]
            && matches!(event.kind, EventKind::RunFinished { .. })
    })
    .await?;
    let specific = tokio::time::timeout(DEADLINE, async {
        loop {
            let result = fixture.manager.status(
                &fixture.owner.session_id,
                &json!({"sessionId":third["sessionId"]}),
            )?;
            if result["status"] == "idle" {
                return Ok::<Value, anyhow::Error>(result);
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    assert_eq!(specific["text"], "answer:status consumption");
    let list = fixture
        .manager
        .status(&fixture.owner.session_id, &json!({}))?;
    let listed = list["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["sessionId"] == third["sessionId"])
        .unwrap();
    assert_eq!(listed["unread"], false);
    let other = fixture.store.create_session("foreign")?;
    assert!(
        fixture
            .manager
            .status(&other.id, &json!({"sessionId":child["sessionId"]}))
            .is_err()
    );
    let foreign = ToolContext {
        session_id: other.id,
        ..fixture.owner.clone()
    };
    assert!(
        fixture
            .manager
            .send(
                &foreign,
                json!({"sessionId":child["sessionId"],"text":"intrusion"})
            )
            .is_err()
    );
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn running_send_steers_same_run_without_a_second_effect() -> Result<()> {
    let mut fixture = Fixture::new(Agents::default(), allow(), None)?;
    let mut events = fixture.parent.subscribe();
    let child = fixture.spawn("gate")?;
    fixture.entered(&child).await?;
    assert_eq!(
        fixture.manager.status(
            &fixture.owner.session_id,
            &json!({"handle":child["handle"]})
        )?["status"],
        "running"
    );
    let sent = fixture.manager.send(
        &fixture.owner,
        json!({"handle":child["handle"],"text":"steered"}),
    )?;
    assert_eq!(sent["status"], "running");
    let accepted = event_matching(&mut events, |event| {
        event.session_id == child["sessionId"]
            && matches!(&event.kind, EventKind::SteeringAccepted { text } if text == "steered")
    })
    .await?;
    assert_eq!(accepted.session_id, child["sessionId"]);
    fixture.probe.release.notify_one();
    let result = fixture.wait(&child).await?;
    assert_eq!(result["status"], "completed");
    assert_eq!(result["text"], "answer:steered");
    let child_id = child["sessionId"].as_str().unwrap();
    assert_eq!(fixture.store.runs(child_id)?.len(), 1);
    assert!(
        fixture
            .store
            .pending_effects(&fixture.owner.session_id)?
            .is_empty()
    );
    assert_eq!(
        fixture
            .store
            .messages(child_id)?
            .iter()
            .filter(|message| message.role == Role::User && message.content == "steered")
            .count(),
        1
    );
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn registered_controls_accept_session_ids_and_running_send_settles_immediately() -> Result<()>
{
    let mut fixture = Fixture::new(Agents::default(), allow(), None)?;
    let child = fixture
        .call("agent", json!({"task":"block"}), RunOptions::default())
        .await?;
    fixture.entered(&child).await?;
    let child_id = child["sessionId"].as_str().unwrap();
    let list = fixture
        .call("agent_status", json!({}), RunOptions::default())
        .await?;
    assert_eq!(list["children"].as_array().unwrap().len(), 1);
    let status = fixture
        .call(
            "agent_status",
            json!({"sessionId":child_id}),
            RunOptions::default(),
        )
        .await?;
    assert_eq!(status["status"], "running");
    let timeout = fixture
        .call(
            "wait",
            json!({"sessionId":child_id,"timeoutMs":0}),
            RunOptions::default(),
        )
        .await?;
    assert_eq!(timeout["status"], "timeout");
    let before_send = fixture.store.pending_effects(&fixture.owner.session_id)?;
    assert_eq!(
        before_send.len(),
        1,
        "only the admitted spawn remains pending"
    );
    let sent = fixture
        .call(
            "agent_send",
            json!({"sessionId":child_id,"text":"steered"}),
            RunOptions::default(),
        )
        .await?;
    assert_eq!(sent["status"], "running");
    assert_eq!(
        fixture.store.pending_effects(&fixture.owner.session_id)?,
        before_send,
        "a running steer records input, not an unresolved external effect"
    );
    let cancelled = fixture
        .call(
            "cancel",
            json!({"sessionId":child_id}),
            RunOptions::default(),
        )
        .await?;
    assert_eq!(cancelled["status"], "cancelled");
    assert!(
        fixture
            .store
            .pending_effects(&fixture.owner.session_id)?
            .is_empty()
    );
    assert_eq!(fixture.store.runs(child_id)?.len(), 1);
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn explicit_limits_bound_depth_active_children_and_retained_handles() -> Result<()> {
    let limits = Agents {
        max_depth: Some(1),
        max_active_per_parent: Some(1),
        max_active_total: Some(2),
        max_retained: Some(2),
        ..Default::default()
    };
    let mut fixture = Fixture::new(limits, allow(), None)?;
    let first = fixture.spawn("block")?;
    fixture.entered(&first).await?;
    assert!(fixture.spawn("overflow").is_err());
    let child_context = ToolContext {
        session_id: first["sessionId"].as_str().unwrap().into(),
        ..fixture.owner.clone()
    };
    assert!(
        fixture
            .manager
            .start(&child_context, json!({"task":"forbidden grandchild"}))
            .is_err()
    );
    let other = fixture.store.create_session("second parent")?;
    let other_context = ToolContext {
        session_id: other.id,
        ..fixture.owner.clone()
    };
    let second = fixture
        .manager
        .start(&other_context, json!({"task":"block"}))?;
    fixture.entered(&second).await?;
    let third = fixture.store.create_session("third parent")?;
    let third_context = ToolContext {
        session_id: third.id,
        ..fixture.owner.clone()
    };
    assert!(
        fixture
            .manager
            .start(&third_context, json!({"task":"host overflow"}))
            .is_err()
    );
    assert_eq!(
        fixture
            .manager
            .cancel(
                &fixture.owner.session_id,
                first["sessionId"].as_str().unwrap()
            )
            .await?["status"],
        "cancelled"
    );
    let replacement = fixture.spawn("replacement")?;
    assert_eq!(fixture.wait(&replacement).await?["status"], "completed");
    assert!(
        fixture
            .manager
            .wait(
                &fixture.owner.session_id,
                first["handle"].as_str().unwrap(),
                Some(0)
            )
            .await
            .is_err()
    );
    // Retention bounds volatile handles, not the canonical archive.
    assert!(
        !fixture
            .store
            .messages(first["sessionId"].as_str().unwrap())?
            .is_empty()
    );
    fixture.manager.shutdown().await?;
    assert_eq!(
        fixture
            .manager
            .wait(
                &other_context.session_id,
                second["sessionId"].as_str().unwrap(),
                Some(0)
            )
            .await?["status"],
        "cancelled"
    );
    Ok(())
}

#[tokio::test]
async fn depth_two_allows_grandchild_and_cancel_waits_for_entire_subtree() -> Result<()> {
    let mut fixture = Fixture::new(
        Agents {
            max_depth: Some(2),
            ..Default::default()
        },
        allow(),
        None,
    )?;
    let child = fixture.spawn("block")?;
    fixture.entered(&child).await?;
    let child_id = child["sessionId"].as_str().unwrap();
    let context = ToolContext {
        session_id: child_id.into(),
        ..fixture.owner.clone()
    };
    let grandchild = fixture.manager.start(&context, json!({"task":"block"}))?;
    fixture.entered(&grandchild).await?;
    let grandchild_id = grandchild["sessionId"].as_str().unwrap();
    let grandchild_context = ToolContext {
        session_id: grandchild_id.into(),
        ..fixture.owner.clone()
    };
    assert!(
        fixture
            .manager
            .start(&grandchild_context, json!({"task":"too deep"}))
            .is_err()
    );
    assert_eq!(
        fixture
            .store
            .session(grandchild_id)?
            .parent_session_id
            .as_deref(),
        Some(child_id)
    );
    assert_eq!(
        fixture.store.chat_path(child_id)?,
        fixture.store.chat_path(grandchild_id)?
    );
    let result = fixture
        .manager
        .cancel(&fixture.owner.session_id, child["handle"].as_str().unwrap())
        .await?;
    assert_eq!(result["status"], "cancelled");
    // A returned cancellation is proof of canonical terminal descendants,
    // not merely proof that their cancellation tokens were signalled.
    for id in [child_id, grandchild_id] {
        let runs = fixture.store.runs(id)?;
        assert_eq!(runs[0].status, "cancelled");
        assert!(runs[0].ended_at.is_some());
    }
    assert_eq!(
        fixture
            .manager
            .wait(child_id, grandchild_id, Some(0))
            .await?["status"],
        "cancelled"
    );
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn permissive_profile_cannot_bypass_parent_ask_and_parent_host_decides_child() -> Result<()> {
    let fixture = Fixture::new(Agents::default(), ApprovalPolicy::default(), Some(allow()))?;
    let mut events = fixture.parent.subscribe();
    let child = fixture.spawn("effect")?;
    let requested = event_matching(&mut events, |event| {
        matches!(event.kind, EventKind::ApprovalRequested { .. })
    })
    .await?;
    assert_eq!(requested.session_id, child["sessionId"]);
    let EventKind::ApprovalRequested { approval } = requested.kind else {
        unreachable!()
    };
    assert_eq!(approval.tool, "effect");
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 0);
    assert!(
        fixture
            .store
            .pending_effects(child["sessionId"].as_str().unwrap())?
            .is_empty()
    );
    fixture.parent.decide(&approval.id, Decision::Approve)?;
    assert_eq!(fixture.wait(&child).await?["status"], "completed");
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 1);
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn strict_profile_tightens_parent_allow() -> Result<()> {
    let deny = ApprovalPolicy {
        confirm: ApprovalMode::Deny,
        ..Default::default()
    };
    let fixture = Fixture::new(Agents::default(), allow(), Some(deny))?;
    let child = fixture.spawn("effect")?;
    assert_eq!(fixture.wait(&child).await?["status"], "completed");
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 0);
    assert!(
        fixture
            .store
            .messages(child["sessionId"].as_str().unwrap())?
            .iter()
            .any(|message| message.role == Role::Tool
                && message.is_error
                && message.content.contains("Denied"))
    );
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn parent_run_override_governs_spawn_and_each_idle_send_effect() -> Result<()> {
    let fixture = Fixture::new(Agents::default(), allow(), Some(allow()))?;
    let mut events = fixture.parent.subscribe();
    // The host default and profile both allow. Only this run asks.
    let child = fixture
        .call(
            "agent",
            json!({"task":"effect"}),
            RunOptions {
                approval: Some(ApprovalPolicy::default()),
                ..Default::default()
            },
        )
        .await?;
    let requested = event_matching(&mut events, |event| {
        event.session_id == child["sessionId"]
            && matches!(event.kind, EventKind::ApprovalRequested { .. })
    })
    .await?;
    let EventKind::ApprovalRequested { approval } = requested.kind else {
        unreachable!()
    };
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture
            .store
            .pending_effects(&fixture.owner.session_id)?
            .len(),
        1
    );
    fixture.parent.decide(&approval.id, Decision::Approve)?;
    assert_eq!(fixture.wait(&child).await?["status"], "completed");
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 1);
    assert!(
        fixture
            .store
            .pending_effects(&fixture.owner.session_id)?
            .is_empty()
    );
    // A later send supplies that parent's current effective override too.
    let sent = fixture
        .call(
            "agent_send",
            json!({"sessionId":child["sessionId"],"text":"effect"}),
            RunOptions {
                approval: Some(ApprovalPolicy {
                    confirm: ApprovalMode::Deny,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(sent["sessionId"], child["sessionId"]);
    assert_eq!(fixture.wait(&child).await?["status"], "completed");
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture
            .store
            .runs(child["sessionId"].as_str().unwrap())?
            .len(),
        2
    );
    assert!(
        fixture
            .store
            .pending_effects(&fixture.owner.session_id)?
            .is_empty()
    );
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn per_tool_clamp_preserves_parent_timeout_and_resolves_mixed_defaults() -> Result<()> {
    let mut parent = ApprovalPolicy {
        confirm: ApprovalMode::Deny,
        timeout_secs: Some(37),
        ..Default::default()
    };
    parent.tools.insert("effect".into(), ApprovalMode::Ask);
    let fixture = Fixture::new(
        Agents::default(),
        parent,
        Some(ApprovalPolicy {
            timeout_secs: Some(99),
            ..allow()
        }),
    )?;
    let mut events = fixture.parent.subscribe();
    let child = fixture.spawn("effect")?;
    let requested = event_matching(&mut events, |event| {
        event.session_id == child["sessionId"]
            && matches!(event.kind, EventKind::ApprovalRequested { .. })
    })
    .await?;
    let EventKind::ApprovalRequested { approval } = requested.kind else {
        unreachable!()
    };
    fixture.parent.decide(&approval.id, Decision::Approve)?;
    fixture.wait(&child).await?;
    {
        let policies = fixture.policies.lock().unwrap();
        assert_eq!(policies.len(), 1);
        assert_eq!(
            policies[0].mode("effect", Approval::Confirm),
            ApprovalMode::Ask,
            "the explicit parent tool rule wins over its stricter default and permissive profile"
        );
        assert_eq!(policies[0].timeout_secs, Some(37));
    }
    let mut next_owner = fixture.owner.clone();
    next_owner.approval.timeout_secs = Some(17);
    fixture.manager.send(
        &next_owner,
        json!({"sessionId":child["sessionId"],"text":"effect"}),
    )?;
    let requested = event_matching(&mut events, |event| {
        event.session_id == child["sessionId"]
            && matches!(event.kind, EventKind::ApprovalRequested { .. })
    })
    .await?;
    let EventKind::ApprovalRequested { approval } = requested.kind else {
        unreachable!()
    };
    fixture.parent.decide(&approval.id, Decision::Approve)?;
    fixture.wait(&child).await?;
    assert_eq!(
        fixture.policies.lock().unwrap()[1].timeout_secs,
        Some(17),
        "each send inherits the current parent timeout"
    );
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn restart_marks_parent_and_child_unknown_reattaches_and_send_never_replays() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (parent_id, child_id, chat) = {
        let store = Store::open_data_dir(directory.path())?;
        let parent = store.create_session("interrupted parent")?;
        let child = store.create_session_with_parent(
            "interrupted child",
            Default::default(),
            Some(&parent.id),
        )?;
        let pending = [
            (&parent.id, "spawn-interrupted", "agent"),
            (&child.id, "effect-interrupted", "effect"),
        ]
        .into_iter()
        .map(|(session, call_id, name)| Message {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session.clone(),
            role: Role::Assistant,
            origin: None,
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: call_id.into(),
                name: name.into(),
                arguments: json!({}),
            }],
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: "2026-10-13T00:00:00Z".into(),
        })
        .collect::<Vec<_>>();
        store.append_batch(&pending)?;
        (
            parent.id.clone(),
            child.id,
            store
                .chat_path(&parent.id)?
                .expect("chat-file fixture has a canonical archive"),
        )
    };
    // Freeze precisely the dispatched boundary. No real external operation is
    // performed or interrupted, and no running runtime survives the restart.
    let conn = rusqlite::Connection::open(&chat)?;
    for (session, run_id, call_id, name) in [
        (
            &parent_id,
            "parent-interrupted",
            "spawn-interrupted",
            "agent",
        ),
        (
            &child_id,
            "child-interrupted",
            "effect-interrupted",
            "effect",
        ),
    ] {
        let run = json!({"id":run_id,"session_id":session,"model":"scripted","status":"running","text":null,"error":null,"started_at":"2026-10-13T00:00:00Z","ended_at":null,"usage":null});
        conn.execute(
            "INSERT INTO runs(id,session_id,status,data) VALUES(?1,?2,'running',?3)",
            rusqlite::params![run_id, session, run.to_string()],
        )?;
        let call = json!({"id":call_id,"name":name,"arguments":{}});
        conn.execute("INSERT INTO effects(id,session_id,run_id,call_id,state,detail) VALUES(?1,?2,?3,?1,'dispatched',?4)", rusqlite::params![call_id,session,run_id,call.to_string()])?;
    }
    drop(conn);
    let store = Store::open_data_dir(directory.path())?;
    for id in [&parent_id, &child_id] {
        assert_eq!(store.pending_effects(id)?[0]["state"], "unknown");
        assert!(
            store
                .messages(id)?
                .iter()
                .any(|message| message.is_error && message.content.contains("outcome unknown"))
        );
    }
    assert_eq!(store.run("child-interrupted")?.status, "interrupted");
    let (entered, _receiver) = mpsc::unbounded_channel();
    let probe = Arc::new(Probe {
        requests: Mutex::default(),
        entered,
        release: Notify::new(),
    });
    let effects = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(CountedEffect {
        calls: effects.clone(),
        policies: Arc::default(),
    }))?;
    let child_harness = Harness::new(
        store.clone(),
        probe.clone(),
        tools,
        HarnessConfig::default(),
    );
    let manager = Arc::new(AgentTools::new(store.clone(), child_harness, None));
    let owner = ToolContext {
        session_id: parent_id.clone(),
        run_id: "restart-parent".into(),
        invocation_id: "resume-send".into(),
        cancellation: CancellationToken::new(),
        approval: allow(),
    };
    let discovered = manager.status(&parent_id, &json!({}))?;
    assert_eq!(discovered["children"].as_array().unwrap().len(), 1);
    assert_eq!(discovered["children"][0]["sessionId"], child_id);
    let reattached = manager.status(&parent_id, &json!({"sessionId":child_id}))?;
    assert!(reattached["handle"].as_str().unwrap().starts_with("agent:"));
    // Session ids, rather than lost pre-restart handles, recover ownership.
    let foreign = store.create_session("foreign owner")?;
    assert!(
        manager
            .status(&foreign.id, &json!({"sessionId":child_id}))
            .is_err()
    );
    let sent = manager.send(&owner, json!({"sessionId":child_id,"text":"resume"}))?;
    assert_eq!(sent["sessionId"], child_id);
    let result = manager.wait(&parent_id, &child_id, Some(5000)).await?;
    assert_eq!(result["status"], "completed");
    assert_eq!(result["text"], "answer:resume");
    assert_eq!(
        effects.load(Ordering::SeqCst),
        0,
        "unknown effect must never be replayed"
    );
    assert_eq!(
        store
            .list_sessions()?
            .iter()
            .filter(|session| session.parent_session_id.as_deref() == Some(&parent_id))
            .count(),
        1
    );
    assert!(probe.requests.lock().unwrap().iter().any(|request| {
        request
            .messages
            .iter()
            .any(|message| message.is_error && message.content.contains("outcome unknown"))
    }));
    assert_eq!(store.pending_effects(&child_id)?[0]["state"], "unknown");
    assert_eq!(store.pending_effects(&parent_id)?[0]["state"], "unknown");
    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn reattached_real_child_keeps_original_profile_capabilities() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let parent = store.create_session("profile restoration parent")?;
    let (entered, _receiver) = mpsc::unbounded_channel();
    let probe = Arc::new(Probe {
        requests: Mutex::default(),
        entered,
        release: Notify::new(),
    });
    let effects = Arc::new(AtomicUsize::new(0));
    let profiles = || -> Result<Vec<AgentProfile>> {
        let mut unrestricted = ToolRegistry::new();
        unrestricted.register(Arc::new(CountedEffect {
            calls: effects.clone(),
            policies: Arc::default(),
        }))?;
        Ok(vec![
            AgentProfile {
                name: "read-only".into(),
                description: "default effect-enabled probe".into(),
                harness: Arc::new(Harness::new(
                    store.clone(),
                    probe.clone(),
                    unrestricted,
                    HarnessConfig {
                        approval: allow(),
                        ..Default::default()
                    },
                )),
                model: None,
                max_rounds: Some(12),
                approval: Some(allow()),
            },
            AgentProfile {
                name: "restricted".into(),
                description: "no effect capability".into(),
                harness: Arc::new(Harness::new(
                    store.clone(),
                    probe.clone(),
                    ToolRegistry::new(),
                    HarnessConfig {
                        approval: allow(),
                        ..Default::default()
                    },
                )),
                model: None,
                max_rounds: Some(12),
                approval: Some(allow()),
            },
        ])
    };
    let owner = ToolContext {
        session_id: parent.id.clone(),
        run_id: "profile-parent".into(),
        invocation_id: "profile-spawn".into(),
        cancellation: CancellationToken::new(),
        approval: allow(),
    };
    let manager = AgentTools::from_profiles(store.clone(), profiles()?)?;
    let child = manager.start(&owner, json!({"task":"first","profile":"restricted"}))?;
    let child_id = child["sessionId"].as_str().unwrap();
    assert_eq!(
        manager.wait(&parent.id, child_id, Some(5000)).await?["status"],
        "completed"
    );
    drop(manager);
    // A new registry must resolve the persisted profile, even though its safe
    // default has different capabilities and permits this synthetic effect.
    let manager = AgentTools::from_profiles(store.clone(), profiles()?)?;
    let listed = manager.status(&parent.id, &json!({}))?;
    assert_eq!(listed["children"][0]["sessionId"], child_id);
    let restored = manager.status(&parent.id, &json!({"sessionId":child_id}))?;
    assert_eq!(restored["text"], "answer:first");
    let restored = manager.wait(&parent.id, child_id, Some(0)).await?;
    assert_eq!(
        restored["text"], "answer:first",
        "reattachment restores the original final answer from canonical history"
    );
    manager.send(&owner, json!({"sessionId":child_id,"text":"effect"}))?;
    assert_eq!(
        manager.wait(&parent.id, child_id, Some(5000)).await?["status"],
        "completed"
    );
    assert_eq!(
        effects.load(Ordering::SeqCst),
        0,
        "reattachment must not substitute the default profile"
    );
    assert!(
        store
            .messages(child_id)?
            .iter()
            .any(|message| message.role == Role::Tool
                && message.is_error
                && message.content.contains("effect"))
    );
    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn consumed_result_stays_read_after_volatile_registry_replacement() -> Result<()> {
    let fixture = Fixture::new(Agents::default(), allow(), None)?;
    let child = fixture.spawn("receipt")?;
    fixture.wait(&child).await?;
    let replacement = AgentTools::from_profiles(
        fixture.store.clone(),
        vec![fixture.manager.profile("read-only").unwrap().clone()],
    )?;
    let listed = replacement.status(&fixture.owner.session_id, &json!({}))?;
    assert_eq!(listed["children"][0]["sessionId"], child["sessionId"]);
    assert_eq!(
        listed["children"][0]["unread"], false,
        "consuming a result must survive loss of its handle"
    );
    fixture.manager.shutdown().await?;
    replacement.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn cancelled_idle_child_can_continue_after_eviction_or_restart() -> Result<()> {
    let limits = Agents {
        max_retained: Some(1),
        ..Default::default()
    };
    let fixture = Fixture::new(limits.clone(), allow(), None)?;
    let child = fixture.spawn("idle before cancellation")?;
    fixture.wait(&child).await?;
    let child_id = child["sessionId"].as_str().unwrap();
    assert_eq!(
        fixture
            .manager
            .cancel(&fixture.owner.session_id, child_id)
            .await?["status"],
        "cancelled"
    );
    let next = fixture.spawn("evicts cancelled handle")?;
    fixture.wait(&next).await?;
    assert_eq!(
        fixture
            .manager
            .status(&fixture.owner.session_id, &json!({"sessionId":child_id}))?["status"],
        "cancelled"
    );
    fixture.manager.send(
        &fixture.owner,
        json!({"sessionId":child_id,"text":"continue after cancellation"}),
    )?;
    assert_eq!(
        fixture
            .manager
            .wait(&fixture.owner.session_id, child_id, Some(5000))
            .await?["status"],
        "completed"
    );
    let replacement = AgentTools::from_profiles(
        fixture.store.clone(),
        vec![fixture.manager.profile("read-only").unwrap().clone()],
    )?
    .with_limits(limits);
    assert_eq!(
        replacement.status(&fixture.owner.session_id, &json!({"sessionId":child_id}))?["status"],
        "idle"
    );
    replacement.send(
        &fixture.owner,
        json!({"sessionId":child_id,"text":"continue after restart"}),
    )?;
    assert_eq!(
        replacement
            .wait(&fixture.owner.session_id, child_id, Some(5000))
            .await?["status"],
        "completed"
    );
    assert_eq!(fixture.store.runs(child_id)?.len(), 3);
    fixture.manager.shutdown().await?;
    replacement.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn parent_cancel_covers_idle_descendants_absent_from_retained_handles() -> Result<()> {
    let limits = Agents {
        max_depth: Some(2),
        max_retained: Some(1),
        ..Default::default()
    };
    let fixture = Fixture::new(limits.clone(), allow(), None)?;
    let child = fixture.spawn("idle branch")?;
    fixture.wait(&child).await?;
    let child_id = child["sessionId"].as_str().unwrap();
    let child_owner = ToolContext {
        session_id: child_id.into(),
        ..fixture.owner.clone()
    };
    let leaf = fixture
        .manager
        .start(&child_owner, json!({"task":"idle leaf"}))?;
    let leaf_id = leaf["sessionId"].as_str().unwrap();
    assert_eq!(
        fixture.manager.wait(child_id, leaf_id, Some(5000)).await?["status"],
        "completed"
    );
    let retainer = fixture.spawn("evicts idle leaf")?;
    fixture.wait(&retainer).await?;
    assert!(
        fixture
            .manager
            .wait(child_id, leaf["handle"].as_str().unwrap(), Some(0))
            .await
            .is_err()
    );
    assert!(fixture.parent.cancel(&fixture.owner.session_id));
    let replacement = AgentTools::from_profiles(
        fixture.store.clone(),
        vec![fixture.manager.profile("read-only").unwrap().clone()],
    )?
    .with_limits(limits);
    assert_eq!(
        replacement.status(&fixture.owner.session_id, &json!({"sessionId":child_id}))?["status"],
        "cancelled"
    );
    assert_eq!(
        replacement.status(child_id, &json!({"sessionId":leaf_id}))?["status"],
        "cancelled"
    );
    replacement.send(
        &child_owner,
        json!({"sessionId":leaf_id,"text":"continue cancelled subtree"}),
    )?;
    assert_eq!(
        replacement
            .wait(&child_owner.session_id, leaf_id, Some(5000))
            .await?["status"],
        "completed"
    );
    fixture.manager.shutdown().await?;
    replacement.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn registered_send_rejections_settle_without_unknown_effects() -> Result<()> {
    let fixture = Fixture::new(Agents::default(), allow(), None)?;
    let child = fixture
        .call(
            "agent",
            json!({"task":"idle send target"}),
            RunOptions::default(),
        )
        .await?;
    fixture.wait(&child).await?;
    let child_id = child["sessionId"].as_str().unwrap();
    fixture
        .manager
        .cancel(&fixture.owner.session_id, child_id)
        .await?;
    for input in [
        json!({"sessionId":child_id,"text":"invalid budget must reject","max_tool_calls":-1}),
        json!({"sessionId":child_id,"text":"x".repeat(64*1024+1)}),
    ] {
        let run = tokio::time::timeout(
            DEADLINE,
            fixture.parent.prompt(
                &fixture.owner.session_id,
                format!("CALL:{}", json!({"target":"agent_send","input":input})),
                RunOptions::default(),
            ),
        )
        .await??;
        assert_eq!(run.status, "completed");
        let messages = fixture.store.messages(&fixture.owner.session_id)?;
        let rejected = messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Tool)
            .unwrap();
        assert!(
            rejected.is_error,
            "send rejection must reach the provider: {}",
            rejected.content
        );
        assert!(
            fixture
                .store
                .pending_effects(&fixture.owner.session_id)?
                .is_empty(),
            "ordinary send rejection has no external outcome to recover"
        );
    }
    assert_eq!(fixture.store.runs(child_id)?.len(), 1);
    fixture.manager.shutdown().await?;
    Ok(())
}

const TREE_DEADLINE: Duration = Duration::from_secs(10);

/// Scripted JS tree node. Task `tree:<levels>` spawns three children in one cell
/// while levels remain, leaving a persistent environment on each non-leaf.
/// A node waits in its provider after the cell returns all three spawn receipts.
struct TreeProbe {
    entered: mpsc::UnboundedSender<String>,
}
#[async_trait]
impl Provider for TreeProbe {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let (index, user) = request
            .messages
            .iter()
            .enumerate()
            .rev()
            .find(|(_, message)| message.role == Role::User)
            .unwrap();
        let levels: usize = user.content.strip_prefix("tree:").unwrap().parse()?;
        let answered = request.messages[index + 1..]
            .iter()
            .filter(|message| message.role == Role::Tool)
            .count();
        if levels > 0 && answered == 0 {
            let task = serde_json::to_string(&format!("tree:{}", levels - 1))?;
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments: json!({"code":format!("state.marker = 'live'; return await Promise.all([1,2,3].map(() => tools.agent({{task:{task}}}))); ")}),
                }],
                ..Default::default()
            });
        }
        self.entered.send(request.session_id.clone())?;
        cancellation.cancelled().await;
        anyhow::bail!("tree node cancelled")
    }
}

/// Canonical descendants of `root` with their depth below it, from the catalog.
fn subtree(store: &Store, root: &str) -> Result<Vec<(String, usize)>> {
    let sessions = store.list_sessions()?;
    let mut found = Vec::new();
    let mut frontier = vec![(root.to_owned(), 0usize)];
    while let Some((parent, depth)) = frontier.pop() {
        for session in sessions
            .iter()
            .filter(|session| session.parent_session_id.as_deref() == Some(parent.as_str()))
        {
            found.push((session.id.clone(), depth + 1));
            frontier.push((session.id.clone(), depth + 1));
        }
    }
    Ok(found)
}

fn direct_children(store: &Store, parent: &str) -> Result<usize> {
    Ok(store
        .list_sessions()?
        .iter()
        .filter(|session| session.parent_session_id.as_deref() == Some(parent))
        .count())
}

fn environment_count(repl: &CodeMode, session: &str) -> usize {
    repl.status(session)["environments"]
        .as_array()
        .unwrap()
        .len()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_host_tree_runs_at_once_and_root_cancel_cleans_every_js_environment()
-> Result<()> {
    const WIDTH: usize = 3;
    const LEVELS: usize = 3;
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let (entered_tx, mut entered) = mpsc::unbounded_channel();
    let provider = Arc::new(TreeProbe {
        entered: entered_tx,
    });
    let repl = Arc::new(CodeMode::new(
        directory.path().to_owned(),
        Arc::new(ArtifactStore::new(
            directory.path(),
            directory.path().join("artifacts"),
        )?),
    )?);
    let config = HarnessConfig {
        execution_mode: bubblepaw_core::ExecutionMode::Js,
        approval: allow(),
        ..Default::default()
    };
    // Production order: base child tools, then the host builds the child
    // manager with default, unlimited `Agents`.
    let mut base = ToolRegistry::new();
    let processes = register_native_tools(&mut base, directory.path(), false)?;
    let manager = child_agent_host(
        store.clone(),
        provider.clone(),
        &base,
        &repl,
        &config,
        directory.path(),
        false,
        None,
        Agents::default(),
        Arc::new(AgentRegistry::default()),
        processes,
    )?;
    // Root tools are installed as the host does: the child tools, handles,
    // and JS environment tools.
    let mut root_tools = ToolRegistry::new();
    manager.register(&mut root_tools)?;
    register_agent_handles(&mut root_tools, manager.clone())?;
    repl.register(&mut root_tools)?;
    let parent = Harness::new(store.clone(), provider.clone(), root_tools, config.clone());
    let root = store.create_session("tree root")?;
    parent.start(&root.id, "tree:3", RunOptions::default())?;

    let total: usize = (1..=LEVELS).map(|depth| WIDTH.pow(depth as u32)).sum();
    assert_eq!(total, 39);
    let mut blocked = std::collections::HashSet::new();
    for _ in 0..=total {
        blocked.insert(
            tokio::time::timeout(TREE_DEADLINE, entered.recv())
                .await?
                .unwrap(),
        );
    }
    let tree = subtree(&store, &root.id)?;
    assert_eq!(tree.len(), total);
    let mut expected: std::collections::HashSet<String> =
        tree.iter().map(|(id, _)| id.clone()).collect();
    expected.insert(root.id.clone());
    assert_eq!(
        blocked, expected,
        "root and every descendant reached its provider wait"
    );
    assert_eq!(store.runs(&root.id)?[0].status, "running");
    assert_eq!(direct_children(&store, &root.id)?, WIDTH);
    for depth in 1..=LEVELS {
        assert_eq!(
            tree.iter().filter(|(_, d)| *d == depth).count(),
            WIDTH.pow(depth as u32)
        );
    }
    let mut live_environments = 0;
    for (id, depth) in &tree {
        assert_eq!(
            store.runs(id)?[0].status,
            "running",
            "depth {depth} node runs alongside the others"
        );
        if *depth < LEVELS {
            assert_eq!(direct_children(&store, id)?, WIDTH);
            // Non-leaf nodes hold one persistent JS environment while blocked.
            assert_eq!(environment_count(&repl, id), 1, "depth {depth} environment");
            live_environments += 1;
        } else {
            assert_eq!(environment_count(&repl, id), 0);
        }
    }
    assert_eq!(live_environments, 12);

    // Cancel from the root harness first, then await the whole subtree.
    assert!(parent.cancel(&root.id));
    tokio::time::timeout(TREE_DEADLINE, parent.wait_for_idle(&root.id)).await??;
    manager.cancel_children(&root.id).await?;
    assert!(
        store.pending_effects(&root.id)?.is_empty(),
        "root spawn effects settle once every descendant cleaned up"
    );
    let root_runs = store.runs(&root.id)?;
    assert_eq!(root_runs[0].status, "cancelled");
    assert!(root_runs[0].ended_at.is_some());
    // Root heaps remain available for a later user turn; child heaps belong to
    // their active turns and must all close before cancellation returns.
    assert_eq!(environment_count(&repl, &root.id), 1);
    for (id, _) in &tree {
        let runs = store.runs(id)?;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "cancelled");
        assert!(runs[0].ended_at.is_some(), "cleanup awaited for {id}");
        assert!(
            store.pending_effects(id)?.is_empty(),
            "no pending or unknown effect after successful cleanup: {id}"
        );
        assert_eq!(
            environment_count(&repl, id),
            0,
            "JS environments closed before cancellation returned: {id}"
        );
    }
    let listed = manager.status(&root.id, &json!({}))?;
    let listed_tree = listed["children"].as_array().unwrap();
    assert_eq!(listed_tree.len(), total);
    assert_eq!(
        listed_tree
            .iter()
            .filter(|child| child["parentSessionId"] == root.id)
            .count(),
        WIDTH
    );
    let listed_ids = listed_tree
        .iter()
        .map(|child| child["sessionId"].as_str().unwrap().to_owned())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        listed_ids,
        tree.iter()
            .map(|(id, _)| id.clone())
            .collect::<std::collections::HashSet<_>>()
    );
    for child in listed_tree {
        assert_eq!(child["status"], "cancelled");
        let session = store.session(child["sessionId"].as_str().unwrap())?;
        assert_eq!(child["parentSessionId"], session.parent_session_id.unwrap());
    }
    manager.shutdown().await?;
    repl.shutdown().await;
    assert_eq!(environment_count(&repl, &root.id), 0);
    Ok(())
}

#[tokio::test]
async fn low_retention_keeps_evicted_children_addressable_and_terminal_states_durable() -> Result<()>
{
    // One retained handle: each spawn evicts the previous childless idle handle.
    let fixture = Fixture::new(
        Agents {
            max_depth: Some(2),
            max_retained: Some(1),
            ..Default::default()
        },
        allow(),
        None,
    )?;
    let owner = fixture.owner.session_id.clone();
    let first = fixture.spawn("first answer")?;
    let original = first["handle"].as_str().unwrap().to_owned();
    fixture.wait(&first).await?;
    let second = fixture.spawn("second evicts first")?;
    fixture.wait(&second).await?;
    // `first` lost its handle, but listing still shows its canonical session.
    let listed = fixture.manager.status(&owner, &json!({}))?;
    let evicted = listed["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["sessionId"] == first["sessionId"])
        .expect("evicted idle child stays listed");
    assert!(evicted["handle"].is_null());
    assert_eq!(evicted["status"], "idle");
    // The original handle no longer addresses anything once evicted.
    assert!(
        fixture
            .manager
            .wait(&owner, &original, Some(0))
            .await
            .is_err()
    );
    assert!(
        fixture
            .manager
            .status(&owner, &json!({"handle": original}))
            .is_err()
    );
    // A specific status reattaches it by sessionId, evicting `second` in turn.
    let status = fixture
        .manager
        .status(&owner, &json!({"sessionId": first["sessionId"]}))?;
    assert_eq!(status["status"], "idle");
    assert_eq!(status["text"], "answer:first answer");
    let fresh = status["handle"].as_str().unwrap().to_owned();
    assert!(fresh.starts_with("agent:"));
    assert_ne!(fresh, original, "reattachment issues a fresh handle");
    assert!(
        fixture
            .manager
            .status(&owner, &json!({"handle": original}))
            .is_err()
    );
    // Evict it again; wait then addresses it by sessionId alone.
    let third = fixture.spawn("third evicts first")?;
    fixture.wait(&third).await?;
    assert!(
        fixture
            .manager
            .status(&owner, &json!({"handle": fresh}))
            .is_err(),
        "the evicted fresh handle fails too"
    );
    let waited = fixture.wait(&first).await?;
    assert_eq!(waited["status"], "completed");
    assert_eq!(waited["text"], "answer:first answer");
    let refreshed = waited["handle"].as_str().unwrap().to_owned();
    assert_ne!(refreshed, fresh, "each reattachment has its own handle");
    assert_ne!(refreshed, original);
    assert!(
        fixture
            .manager
            .wait(&owner, &original, Some(0))
            .await
            .is_err()
    );
    // Evict once more; send by sessionId reattaches and starts a new turn.
    let fourth = fixture.spawn("fourth evicts first")?;
    fixture.wait(&fourth).await?;
    assert!(
        fixture
            .manager
            .send(
                &fixture.owner,
                json!({"handle": refreshed, "text": "stale handle"})
            )
            .is_err()
    );
    let sent = fixture.manager.send(
        &fixture.owner,
        json!({"sessionId": first["sessionId"], "text": "again"}),
    )?;
    assert_eq!(sent["status"], "running");
    let again = fixture.wait(&first).await?;
    assert_eq!(again["status"], "completed");
    assert_eq!(again["text"], "answer:again");

    // A cancelled child keeps its terminal state after its handle is evicted.
    let victim = fixture.spawn("victim")?;
    fixture.wait(&victim).await?;
    assert_eq!(
        fixture
            .manager
            .cancel(&owner, victim["sessionId"].as_str().unwrap())
            .await?["status"],
        "cancelled"
    );
    let evictor = fixture.spawn("evicts cancelled victim")?;
    fixture.wait(&evictor).await?;
    let listed = fixture.manager.status(&owner, &json!({}))?;
    let entry = listed["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["sessionId"] == victim["sessionId"])
        .expect("evicted cancelled child stays listed");
    assert!(entry["handle"].is_null());
    assert_eq!(entry["status"], "cancelled");
    fixture.manager.send(
        &fixture.owner,
        json!({"sessionId": victim["sessionId"], "text": "continue child"}),
    )?;
    assert_eq!(
        fixture
            .manager
            .wait(
                &fixture.owner.session_id,
                victim["sessionId"].as_str().unwrap(),
                Some(5000)
            )
            .await?["status"],
        "completed"
    );
    assert_eq!(
        fixture
            .manager
            .status(&owner, &json!({"sessionId": victim["sessionId"]}))?["status"],
        "idle"
    );

    // A failed child keeps its failed state after its handle is evicted.
    let broken = fixture.spawn("fail")?;
    assert_eq!(fixture.wait(&broken).await?["status"], "failed");
    let evictor = fixture.spawn("evicts failed child")?;
    fixture.wait(&evictor).await?;
    let listed = fixture.manager.status(&owner, &json!({}))?;
    let entry = listed["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["sessionId"] == broken["sessionId"])
        .expect("evicted failed child stays listed");
    assert!(entry["handle"].is_null());
    assert_eq!(entry["status"], "failed");
    fixture.manager.send(
        &fixture.owner,
        json!({"sessionId": broken["sessionId"], "text": "continue child"}),
    )?;
    assert_eq!(
        fixture
            .manager
            .wait(
                &fixture.owner.session_id,
                broken["sessionId"].as_str().unwrap(),
                Some(5000)
            )
            .await?["status"],
        "completed"
    );
    assert_eq!(
        fixture
            .manager
            .status(&owner, &json!({"sessionId": broken["sessionId"]}))?["status"],
        "idle"
    );

    // An idle subtree with every handle evicted is still cancelled by its root.
    let branch = fixture.spawn("idle branch")?;
    fixture.wait(&branch).await?;
    let branch_id = branch["sessionId"].as_str().unwrap().to_owned();
    let branch_owner = ToolContext {
        session_id: branch_id.clone(),
        ..fixture.owner.clone()
    };
    let leaf = fixture
        .manager
        .start(&branch_owner, json!({"task": "idle leaf"}))?;
    let leaf_id = leaf["sessionId"].as_str().unwrap().to_owned();
    assert_eq!(
        fixture
            .manager
            .wait(&branch_id, &leaf_id, Some(5000))
            .await?["status"],
        "completed"
    );
    let evictor = fixture.spawn("evicts idle leaf")?;
    fixture.wait(&evictor).await?;
    assert_eq!(
        fixture.manager.cancel(&owner, &branch_id).await?["status"],
        "cancelled"
    );
    let listed = fixture.manager.status(&branch_id, &json!({}))?;
    let entry = listed["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["sessionId"] == leaf["sessionId"])
        .expect("evicted leaf stays listed under its parent");
    assert!(entry["handle"].is_null());
    assert_eq!(entry["status"], "cancelled");
    fixture.manager.send(
        &branch_owner,
        json!({"sessionId": leaf_id, "text": "continue child"}),
    )?;
    assert_eq!(
        fixture
            .manager
            .wait(&branch_owner.session_id, &leaf_id, Some(5000))
            .await?["status"],
        "completed"
    );
    fixture.manager.shutdown().await?;
    Ok(())
}

#[test]
fn child_approval_denies_union_parent_and_profile() {
    let parent = ApprovalPolicy {
        deny: vec!["exec".into(), "shared".into()],
        ..allow()
    };
    let profile = ApprovalPolicy {
        deny: vec!["write".into(), "shared".into()],
        ..allow()
    };
    let child = bubblepaw_runner::agents::clamp_approval(&parent, Some(&profile));
    assert_eq!(child.deny, ["write", "shared", "exec"]);
    assert_eq!(child.mode("exec", Approval::None), ApprovalMode::Deny);
    assert_eq!(child.mode("write", Approval::None), ApprovalMode::Deny);
    assert_eq!(
        bubblepaw_runner::agents::clamp_approval(&child, None).deny,
        child.deny
    );
}

#[test]
fn child_inherits_the_host_hardline_floor_and_profiles_cannot_drop_it() {
    let floor_off = ApprovalPolicy {
        hardline: false,
        ..allow()
    };
    let profile_off = ApprovalPolicy {
        hardline: false,
        ..allow()
    };
    // The host's floor passes to children, whatever their profile says.
    assert!(bubblepaw_runner::agents::clamp_approval(&allow(), None).hardline);
    assert!(bubblepaw_runner::agents::clamp_approval(&allow(), Some(&profile_off)).hardline);
    assert!(!bubblepaw_runner::agents::clamp_approval(&floor_off, None).hardline);
    assert!(!bubblepaw_runner::agents::clamp_approval(&floor_off, Some(&allow())).hardline);
}

#[tokio::test]
async fn child_denies_from_parent_or_profile_survive_permissive_run_override() -> Result<()> {
    for parent_denies in [false, true] {
        let deny = ApprovalPolicy {
            deny: vec!["effect".into()],
            ..allow()
        };
        let (parent, profile) = if parent_denies {
            (deny, allow())
        } else {
            (allow(), deny)
        };
        let fixture = Fixture::new(Agents::default(), parent, Some(profile))?;
        let child = fixture
            .call(
                "agent",
                json!({"task":"effect"}),
                RunOptions {
                    approval: Some(allow()),
                    ..Default::default()
                },
            )
            .await?;
        assert_eq!(fixture.wait(&child).await?["status"], "completed");
        assert_eq!(fixture.effects.load(Ordering::SeqCst), 0);
        assert!(
            fixture
                .store
                .messages(child["sessionId"].as_str().unwrap())?
                .iter()
                .any(|message| message.role == Role::Tool
                    && message.is_error
                    && message.content.contains("deny rule effect"))
        );
        fixture.manager.shutdown().await?;
    }
    Ok(())
}

const RECURSIVE_DENY_TOOLS: [&str; 5] = [
    "root_effect",
    "override_effect",
    "child_effect",
    "grandchild_effect",
    "allowed_effect",
];

#[derive(Default)]
struct RecursiveDenyEffects {
    calls: Mutex<std::collections::BTreeMap<(String, String), usize>>,
    policies: Mutex<std::collections::BTreeMap<String, ApprovalPolicy>>,
}

struct RecursiveDenyEffect {
    name: &'static str,
    effects: Arc<RecursiveDenyEffects>,
}

#[async_trait]
impl Tool for RecursiveDenyEffect {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: "Counted effect for recursive deny acceptance".into(),
            input_schema: json!({"type":"object"}),
            read_only: false,
            approval: Approval::Confirm,
        }
    }

    async fn invoke(&self, context: ToolContext, _: Value) -> Result<ToolResult> {
        *self
            .effects
            .calls
            .lock()
            .unwrap()
            .entry((context.session_id.clone(), self.name.into()))
            .or_default() += 1;
        self.effects
            .policies
            .lock()
            .unwrap()
            .insert(context.session_id, context.approval);
        Ok(ToolResult::text("effect completed"))
    }
}

struct RecursiveDenyProvider {
    entered: mpsc::UnboundedSender<(usize, String)>,
    profile_additions: bool,
}

#[async_trait]
impl Provider for RecursiveDenyProvider {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let (index, input) = request
            .messages
            .iter()
            .enumerate()
            .rev()
            .find(|(_, message)| {
                message.role == Role::User
                    && message.origin != Some(bubblepaw_core::MessageOrigin::Generated)
            })
            .unwrap();
        let depth: usize = input
            .content
            .strip_prefix("recursive-deny:")
            .unwrap()
            .parse()?;
        if request.messages[index + 1..]
            .iter()
            .any(|message| message.role == Role::Tool)
        {
            return Ok(ModelResponse {
                text: "recursive deny probe done".into(),
                ..Default::default()
            });
        }
        self.entered.send((depth, request.session_id.clone()))?;
        // Each level really dispatches all five effects. Later profile denies
        // must take effect only in that profile and its descendants.
        let mut tool_calls = RECURSIVE_DENY_TOOLS
            .iter()
            .map(|tool| ToolCall {
                id: format!("recursive-deny-{depth}-{tool}"),
                name: (*tool).into(),
                arguments: json!({}),
            })
            .collect::<Vec<_>>();
        if depth < 3 {
            let next_profile = if self.profile_additions && depth > 0 {
                "restricted"
            } else {
                "workspace"
            };
            tool_calls.push(ToolCall {
                id: format!("recursive-deny-{depth}-spawn"),
                name: "bridge".into(),
                arguments: json!({"target":"agent", "input":{
                    "task":format!("recursive-deny:{}", depth + 1),
                    "profile":next_profile
                }}),
            });
        }
        Ok(ModelResponse {
            tool_calls,
            ..Default::default()
        })
    }
}

#[path = "child_sessions/deny_inheritance.rs"]
mod deny_inheritance;
