//! Recursive lifecycle acceptance with scripted providers only.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    Approval, ApprovalMode, CancellationToken, Harness, HarnessConfig, MessageOrigin, ModelRequest,
    ModelResponse, Provider, Role, Store, TextCallback, Tool, ToolCall, ToolContext,
    ToolDefinition, ToolDispatcher, ToolRegistry, ToolResult,
};
use bubblepaw_runner::{
    agents::{AgentProfile, AgentRegistry, AgentTools},
    config::Agents,
};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct Script {
    requests: Mutex<Vec<ModelRequest>>,
    actions: Mutex<HashSet<(String, String)>>,
}
#[async_trait]
impl Provider for Script {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let task = request
            .messages
            .iter()
            .rev()
            .filter(|message| message.role == Role::User)
            .find_map(|message| {
                message
                    .content
                    .lines()
                    .next()
                    .and_then(|line| serde_json::from_str::<Value>(line).ok())
            })
            .unwrap_or(json!({}));
        self.requests.lock().unwrap().push(request.clone());
        let first = self
            .actions
            .lock()
            .unwrap()
            .insert((request.session_id.clone(), task.to_string()));
        let waking = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .is_some_and(|message| message.content.starts_with("<harness "));
        if task["hang"] == true || task["hang-on-wake"] == true && waking {
            cancellation.cancelled().await;
            anyhow::bail!("script cancelled");
        }
        if first && let Some(delay) = task["delay"].as_u64() {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        let mut calls = Vec::new();
        if first && task.get("spawn").is_some() {
            calls.push(("bridge", json!({"tool":"agent","input":task["spawn"]})));
        } else if first && task.get("send").is_some() {
            calls.push(("bridge", json!({"tool":"agent_send","input":task["send"]})));
        } else if first && task["spend"] == true {
            calls.extend((0..5).map(|_| ("noop", json!({}))));
        }
        Ok(ModelResponse {
            text: if calls.is_empty() {
                "script answer".into()
            } else {
                String::new()
            },
            tool_calls: calls
                .into_iter()
                .map(|(name, arguments)| ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: name.into(),
                    arguments,
                })
                .collect(),
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
            description: "scripted JS-equivalent bridge".into(),
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
                input["tool"].as_str().unwrap(),
                input["input"].clone(),
            )
            .await?;
        Ok(ToolResult {
            pending: false,
            ..result
        })
    }
}
struct Noop(Arc<AtomicUsize>);
#[async_trait]
impl Tool for Noop {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "noop".into(),
            description: "script no-op".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Approval::None,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(ToolResult::text("done"))
    }
}
struct Fixture {
    store: Store,
    root: String,
    agents: Arc<AgentTools>,
    script: Arc<Script>,
    calls: Arc<AtomicUsize>,
}
impl Fixture {
    fn new(wake: bool) -> Result<Self> {
        let store = Store::in_memory()?;
        let root = store.create_session("root")?.id;
        let script = Arc::new(Script::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let agents = AgentTools::new_recursive(
            store.clone(),
            Agents {
                auto_wake: wake,
                wake_debounce_ms: 10,
                ..Default::default()
            },
            Arc::new(AgentRegistry::default()),
            None,
            None,
            |manager| {
                let mut tools = ToolRegistry::new();
                manager.register_weak(&mut tools)?;
                tools.register(Arc::new(Bridge))?;
                tools.register(Arc::new(Noop(calls.clone())))?;
                let profile = AgentProfile {
                    name: "read-only".into(),
                    description: "recursive script".into(),
                    harness: Arc::new(Harness::new(
                        store.clone(),
                        script.clone(),
                        tools,
                        HarnessConfig::default(),
                    )),
                    model: None,
                    max_rounds: Some(20),
                    approval: None,
                };
                let mut workspace = profile.clone();
                workspace.name = "workspace".into();
                Ok(vec![profile, workspace])
            },
        )?;
        Ok(Self {
            store,
            root,
            agents,
            script,
            calls,
        })
    }
    fn context(&self, session: &str) -> ToolContext {
        ToolContext {
            session_id: session.into(),
            run_id: self
                .store
                .runs(session)
                .unwrap()
                .last()
                .map(|run| run.id.clone())
                .unwrap_or_default(),
            invocation_id: uuid::Uuid::new_v4().to_string(),
            cancellation: CancellationToken::new(),
            approval: Default::default(),
        }
    }
    async fn wait(&self, owner: &str, child: &str) -> Result<Value> {
        let result = self.agents.wait(owner, child, Some(3000)).await?;
        assert_ne!(result["status"], "timeout", "{result}");
        Ok(result)
    }
    async fn children(&self, count: usize) -> Vec<bubblepaw_core::Session> {
        eventually(|| self.store.list_sessions().unwrap().len() == count + 1).await;
        self.store
            .list_sessions()
            .unwrap()
            .into_iter()
            .filter(|session| session.id != self.root)
            .collect()
    }
    fn controls(&self, session: &str) -> Value {
        let run = self.store.runs(session).unwrap().last().unwrap().id.clone();
        let (metadata, _) = self
            .store
            .attachment(session, &format!("bubblepaw-agent-controls-v1:{run}"))
            .unwrap()
            .unwrap();
        serde_json::from_str(&metadata).unwrap()
    }
}
async fn eventually(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
fn task(value: Value) -> String {
    value.to_string()
}

#[tokio::test]
async fn three_levels_clamp_budget_count_down_and_continue_grandchild() -> Result<()> {
    let fixture = Fixture::new(false)?;
    let leaf = json!({"task":task(json!({})),"max_tool_calls":99});
    let middle = json!({"task":task(json!({"spawn":leaf})),"max_tool_calls":99});
    let first = fixture.agents.start(
        &fixture.context(&fixture.root),
        json!({"task":task(json!({"spawn":middle})),"max_tool_calls":7}),
    )?;
    let sessions = fixture.children(3).await;
    for session in &sessions {
        fixture
            .wait(session.parent_session_id.as_deref().unwrap(), &session.id)
            .await?;
    }
    let first_id = first["sessionId"].as_str().unwrap();
    let middle = sessions
        .iter()
        .find(|session| session.parent_session_id.as_deref() == Some(first_id))
        .unwrap();
    let leaf = sessions
        .iter()
        .find(|session| session.parent_session_id.as_deref() == Some(&middle.id))
        .unwrap();
    assert_eq!(fixture.controls(first_id)["max_tool_calls"], 7);
    assert_eq!(fixture.controls(&middle.id)["max_tool_calls"], 5);
    assert_eq!(fixture.controls(&leaf.id)["max_tool_calls"], 3);
    assert!(
        fixture
            .store
            .messages(first_id)?
            .iter()
            .any(|message| message.role == Role::Tool
                && message.content.contains("5 tool calls left"))
    );
    let list = fixture.agents.status(&fixture.root, &json!({}))?;
    assert_eq!(list["children"].as_array().unwrap().len(), 3);
    assert!(
        list["children"]
            .as_array()
            .unwrap()
            .iter()
            .any(|child| child["sessionId"] == leaf.id && child["parentSessionId"] == middle.id)
    );
    fixture.agents.send(
        &fixture.context(&middle.id),
        json!({"sessionId":leaf.id,"text":task(json!({"follow-up":true}))}),
    )?;
    assert_eq!(
        fixture.wait(&middle.id, &leaf.id).await?["status"],
        "completed"
    );
    assert_eq!(fixture.store.runs(&leaf.id)?.len(), 2);
    fixture.agents.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn continuation_clamps_live_parent_budget_and_exhausts_with_tools_disabled() -> Result<()> {
    let fixture = Fixture::new(false)?;
    let first = fixture.agents.start(
        &fixture.context(&fixture.root),
        json!({"task":task(json!({"spawn":{"task":task(json!({}))}}))}),
    )?;
    let sessions = fixture.children(2).await;
    for session in &sessions {
        fixture
            .wait(session.parent_session_id.as_deref().unwrap(), &session.id)
            .await?;
    }
    let first_id = first["sessionId"].as_str().unwrap();
    let leaf = sessions
        .iter()
        .find(|session| session.parent_session_id.as_deref() == Some(first_id))
        .unwrap();
    fixture.agents.send(&fixture.context(&fixture.root), json!({"sessionId":first_id,"text":task(json!({"send":{"sessionId":leaf.id,"text":task(json!({"spend":true})),"max_tool_calls":99,"timeout":9999}})),"max_tool_calls":4,"timeout":1000}))?;
    fixture.wait(&fixture.root, first_id).await?;
    eventually(|| fixture.store.runs(&leaf.id).unwrap().len() == 2).await;
    assert_eq!(
        fixture.wait(first_id, &leaf.id).await?["status"],
        "completed"
    );
    let controls = fixture.controls(&leaf.id);
    assert_eq!(controls["max_tool_calls"], 2);
    assert!(controls["timeout"].as_u64().unwrap() <= 1000);
    {
        let requests = fixture.script.requests.lock().unwrap();
        let last = requests
            .iter()
            .rev()
            .find(|request| request.session_id == leaf.id)
            .unwrap();
        let text = format!("{:?}", last.messages);
        assert!(!last.tools.is_empty() && text.contains("tools_off:"));
        assert!(text.contains("0 tool calls left"));
        let stored = fixture.store.messages(&leaf.id)?;
        let ran = stored.iter().filter(|m| m.role == Role::Tool).count();
        let sent = last.messages.iter().filter(|m| m.role == Role::Tool);
        assert_eq!(ran, sent.count(), "no tool ran after the tools-off round");
    }
    fixture.agents.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn spawn_timeout_uses_parent_remaining_wall_time() -> Result<()> {
    let fixture = Fixture::new(false)?;
    fixture.agents.start(&fixture.context(&fixture.root), json!({"task":task(json!({"delay":100,"spawn":{"task":task(json!({"hang":true})),"timeout":9999}})),"timeout":600}))?;
    let sessions = fixture.children(2).await;
    let leaf = sessions
        .iter()
        .find(|session| session.parent_session_id.as_deref() != Some(&fixture.root))
        .unwrap();
    let controls = fixture.controls(&leaf.id);
    assert!(controls["timeout"].as_u64().unwrap() < 550, "{controls}");
    assert_eq!(
        fixture
            .wait(leaf.parent_session_id.as_deref().unwrap(), &leaf.id)
            .await?["status"],
        "timed_out"
    );
    fixture.agents.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn ordinary_hidden_child_wakes_worker_but_worker_stays_silent_outward() -> Result<()> {
    let fixture = Fixture::new(true)?;
    let driver = fixture.agents.wake_coordinator().start_driver();
    let mut profile = fixture.agents.profile("read-only").unwrap().clone();
    profile.name = "custom-internal-worker".into();
    let worker = fixture.agents.start_worker(
        &fixture.context(&fixture.root),
        profile,
        json!({"task":task(json!({"spawn":{"task":task(json!({"delay":100}))}}))}),
    )?;
    let worker_id = worker["sessionId"].as_str().unwrap();
    let sessions = fixture.children(2).await;
    assert!(sessions.iter().all(|session| session.hidden));
    eventually(|| fixture.store.runs(worker_id).unwrap().len() == 2).await;
    fixture.wait(&fixture.root, worker_id).await?;
    assert!(fixture.agents.take_reminders(&fixture.root).is_empty());
    assert!(
        fixture
            .store
            .messages(worker_id)?
            .iter()
            .any(|message| message.origin == Some(MessageOrigin::Generated)
                && message.content.starts_with("<harness "))
    );
    assert_eq!(
        fixture.agents.status(worker_id, &json!({}))?["children"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        fixture
            .agents
            .send(
                &fixture.context(&fixture.root),
                json!({"sessionId":worker_id,"text":"again"})
            )
            .is_err()
    );
    drop(driver);
    fixture.agents.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn idle_completion_wakes_only_the_direct_parent() -> Result<()> {
    let fixture = Fixture::new(true)?;
    let driver = fixture.agents.wake_coordinator().start_driver();
    let first = fixture.agents.start(&fixture.context(&fixture.root), json!({"task":task(json!({"hang-on-wake":true,"spawn":{"task":task(json!({"delay":150}))}}))}))?;
    let first_id = first["sessionId"].as_str().unwrap();
    fixture.wait(&fixture.root, first_id).await?;
    let sessions = fixture.children(2).await;
    let leaf = sessions
        .iter()
        .find(|session| session.parent_session_id.as_deref() == Some(first_id))
        .unwrap();
    eventually(|| fixture.store.runs(first_id).unwrap().len() == 2).await;
    assert_eq!(
        fixture.store.runs(first_id)?.last().unwrap().status,
        "running"
    );
    assert!(
        fixture
            .store
            .messages(first_id)?
            .iter()
            .any(|message| message.origin == Some(MessageOrigin::Generated)
                && message.content.starts_with("<harness ")
                && message.content.contains(&leaf.id))
    );
    assert!(fixture.agents.take_reminders(&fixture.root).is_empty());
    assert!(fixture.store.runs(&fixture.root)?.is_empty());
    fixture.agents.cancel(&fixture.root, first_id).await?;
    drop(driver);
    fixture.agents.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn completion_reaches_direct_parent_and_cancellation_cascades_three_levels() -> Result<()> {
    let fixture = Fixture::new(false)?;
    let leaf = json!({"task":task(json!({"hang":true}))});
    let middle = json!({"task":task(json!({"spawn":leaf}))});
    let first = fixture.agents.start(
        &fixture.context(&fixture.root),
        json!({"task":task(json!({"spawn":middle}))}),
    )?;
    let sessions = fixture.children(3).await;
    let first_id = first["sessionId"].as_str().unwrap();
    fixture.wait(&fixture.root, first_id).await?;
    let middle = sessions
        .iter()
        .find(|session| session.parent_session_id.as_deref() == Some(first_id))
        .unwrap();
    eventually(|| {
        fixture.agents.status(first_id, &json!({})).unwrap()["children"]
            .as_array()
            .unwrap()
            .iter()
            .any(|child| child["sessionId"] == middle.id && child["state"] == "idle")
    })
    .await;
    assert!(fixture.agents.take_reminders(&fixture.root).is_empty());
    assert!(
        fixture
            .agents
            .take_reminders(first_id)
            .iter()
            .any(|notice| notice.text.contains(&middle.id))
    );
    fixture.agents.cancel(&fixture.root, first_id).await?;
    for session in sessions {
        assert_eq!(
            fixture.agents.status(
                session.parent_session_id.as_deref().unwrap(),
                &json!({"sessionId":session.id})
            )?["state"],
            "cancelled"
        );
    }
    fixture.agents.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn nested_approvals_and_profile_restrictions_remain_clamped() -> Result<()> {
    let fixture = Fixture::new(false)?;
    let mut context = fixture.context(&fixture.root);
    context
        .approval
        .tools
        .insert("noop".into(), ApprovalMode::Deny);
    fixture.agents.start(
        &context,
        json!({"task":task(json!({"spawn":{"task":task(json!({"spend":true}))}}))}),
    )?;
    let sessions = fixture.children(2).await;
    for session in &sessions {
        fixture
            .wait(session.parent_session_id.as_deref().unwrap(), &session.id)
            .await?;
    }
    assert_eq!(fixture.calls.load(Ordering::Relaxed), 0);
    let leaf = sessions
        .iter()
        .find(|session| session.parent_session_id.as_deref() != Some(&fixture.root))
        .unwrap();
    assert!(
        fixture
            .store
            .messages(&leaf.id)?
            .iter()
            .any(|message| message.role == Role::Tool && message.content.contains("Denied"))
    );
    let child = fixture.agents.start(
        &fixture.context(&fixture.root),
        json!({"task":task(json!({"spawn":{"task":task(json!({})),"profile":"workspace"}}))}),
    )?;
    let child_id = child["sessionId"].as_str().unwrap();
    fixture.wait(&fixture.root, child_id).await?;
    assert!(fixture.store.messages(child_id)?.iter().any(|message| message.role == Role::Tool && message.content.contains("cannot broaden")));
    assert!(
        fixture
            .store
            .list_sessions()?
            .iter()
            .all(|session| session.parent_session_id.as_deref() != Some(child_id))
    );
    fixture.agents.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn explicit_depth_and_active_caps_still_apply() -> Result<()> {
    let fixture = Fixture::new(false)?;
    fixture.agents.apply_limits(&Agents {
        max_depth: Some(1),
        auto_wake: false,
        ..Default::default()
    });
    let child = fixture.agents.start(
        &fixture.context(&fixture.root),
        json!({"task":task(json!({"spawn":{"task":task(json!({}))}}))}),
    )?;
    let child_id = child["sessionId"].as_str().unwrap();
    fixture.wait(&fixture.root, child_id).await?;
    assert!(
        fixture
            .store
            .messages(child_id)?
            .iter()
            .any(|message| message.role == Role::Tool && message.content.contains("depth limit"))
    );
    assert_eq!(fixture.store.list_sessions()?.len(), 2);
    fixture.agents.apply_limits(&Agents {
        max_active_total: Some(1),
        auto_wake: false,
        ..Default::default()
    });
    fixture.agents.start(
        &fixture.context(&fixture.root),
        json!({"task":task(json!({"hang":true}))}),
    )?;
    assert!(
        fixture
            .agents
            .start(
                &fixture.context(&fixture.root),
                json!({"task":task(json!({}))})
            )
            .is_err()
    );
    fixture.agents.shutdown().await?;
    Ok(())
}
