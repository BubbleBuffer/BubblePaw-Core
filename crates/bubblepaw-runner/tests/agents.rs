use anyhow::{Result, bail};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, Role,
    RunOptions, Store, TextCallback, ToolCall, ToolContext, ToolRegistry, Usage,
};
use bubblepaw_runner::{
    agents::AgentTools, artifacts::ArtifactStore, codemode::CodeMode, config::Agents,
    tools::register_native_tools,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Probe(Mutex<Vec<ModelRequest>>);
#[async_trait]
impl Provider for Probe {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let text = request
            .messages
            .iter()
            .find(|message| message.role == Role::User)
            .unwrap()
            .content
            .clone();
        let has_tool = request
            .messages
            .iter()
            .any(|message| message.role == Role::Tool);
        let tool_failed = request
            .messages
            .iter()
            .any(|message| message.role == Role::Tool && message.is_error);
        self.0.lock().unwrap().push(request);
        if tool_failed {
            bail!("child JS failed");
        }
        if text.contains("block") {
            cancellation.cancelled().await;
            bail!("cancelled synthetic provider");
        }
        if text.contains("failure") {
            bail!("synthetic provider failure");
        }
        if text.contains("large") {
            return Ok(ModelResponse {
                text: "\"\\\n\0é".repeat(30_000),
                ..Default::default()
            });
        }
        if text.contains("inspect") && !has_tool {
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments: json!({"code":"if (['agent','edit','write','exec'].some(name=>name in tools)) throw Error('unexpected child capability'); if (state.parent !== undefined) throw Error('inherited parent JS'); state.child=true; const read=await tools.read({path:'explicit.txt'}); text(read.content);"}),
                }],
                ..Default::default()
            });
        }
        if text.contains("workspace child") && !has_tool {
            #[cfg(windows)]
            let command = "Start-Sleep -Seconds 30";
            #[cfg(not(windows))]
            let command = "sleep 30";
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments: json!({"code":format!("if ('agent' in tools) throw Error('recursive child'); await tools.write({{path:'written.txt',content:'explicit workspace child\\n'}}); const p=await tools.exec({}); text(p.handle);", json!({"command":command,"yieldMs":0}))}),
                }],
                ..Default::default()
            });
        }
        Ok(ModelResponse {
            text: if has_tool {
                "child final answer".into()
            } else {
                text
            },
            usage: Some(Usage {
                input_tokens: 3,
                output_tokens: 5,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}
struct Fixture {
    root: tempfile::TempDir,
    store: Store,
    provider: Arc<Probe>,
    repl: Arc<CodeMode>,
    manager: Arc<AgentTools>,
    owner: ToolContext,
}
impl Fixture {
    fn new() -> Self {
        Self::with_parent_strategy(Default::default())
    }
    fn with_parent_strategy(strategy: bubblepaw_core::ContextStrategy) -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("explicit.txt"), "child-only output").unwrap();
        let store = Store::open(root.path().join("archive.sqlite")).unwrap();
        let provider = Arc::new(Probe::default());
        let mut registry = ToolRegistry::new();
        register_native_tools(&mut registry, root.path(), true).unwrap();
        let repl = Arc::new(
            CodeMode::new(
                root.path().to_owned(),
                Arc::new(ArtifactStore::new(root.path(), root.path().join("artifacts")).unwrap()),
            )
            .unwrap(),
        );
        repl.register(&mut registry).unwrap();
        let child = Harness::new(
            store.clone(),
            provider.clone(),
            registry.read_only(),
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                system_prompt: "Child policy".into(),
                ..Default::default()
            },
        );
        // The per-parent and host caps that the limit tests rely on are explicit.
        let manager = Arc::new(
            AgentTools::new(store.clone(), child, None)
                .with_limits(Agents {
                    max_active_per_parent: Some(2),
                    max_active_total: Some(4),
                    max_retained: Some(64),
                    ..Default::default()
                })
                .with_repl(repl.clone()),
        );
        let session = store
            .create_session_with_strategy("parent", strategy)
            .unwrap();
        let owner = ToolContext {
            session_id: session.id,
            run_id: "parent-run".into(),
            invocation_id: "spawn-invocation".into(),
            cancellation: CancellationToken::new(),
            approval: Default::default(),
        };
        Self {
            root,
            store,
            provider,
            repl,
            manager,
            owner,
        }
    }
    fn spawn(&self, task: &str) -> Value {
        self.manager
            .start(&self.owner, json!({"task":task}))
            .unwrap()
    }
    async fn wait(&self, value: &Value) -> Value {
        self.manager
            .wait(
                &self.owner.session_id,
                value["handle"].as_str().unwrap(),
                Some(2000),
            )
            .await
            .unwrap()
    }
    async fn shutdown(&self) {
        self.manager.shutdown().await.unwrap();
        self.repl.shutdown().await;
    }
}

#[tokio::test]
async fn fresh_visible_child_has_explicit_context_only_and_reopens_as_history() {
    let fixture = Fixture::with_parent_strategy(bubblepaw_core::ContextStrategy::Classical);
    // Give the parent real canonical history. It must never enter a child request.
    let parent = Harness::new(
        fixture.store.clone(),
        fixture.provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    );
    parent
        .prompt(
            &fixture.owner.session_id,
            "private parent history",
            RunOptions::default(),
        )
        .await
        .unwrap();
    let started = fixture
        .manager
        .start(
            &fixture.owner,
            json!({"task":"answer using supplied context","context":{"note":"explicit handoff"}}),
        )
        .unwrap();
    assert_eq!(started["status"], "running");
    assert!(started["handle"].as_str().unwrap().starts_with("agent:"));
    let result = fixture.wait(&started).await;
    assert_eq!(result["status"], "completed");
    assert_eq!(
        result["usage"],
        json!({"input_tokens":3,"output_tokens":5,"cache_read_tokens":0,"cache_creation_tokens":0})
    );
    let child_id = started["sessionId"].as_str().unwrap();
    let request = fixture
        .provider
        .0
        .lock()
        .unwrap()
        .iter()
        .find(|request| request.system_prompt == "Child policy")
        .unwrap()
        .clone();
    assert_eq!(
        request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["js"]
    );
    assert_eq!(request.messages.len(), 1);
    assert!(request.messages[0].content.contains("explicit handoff"));
    assert!(
        !request.messages[0]
            .content
            .contains("private parent history")
    );
    let child_session = fixture.store.session(child_id).unwrap();
    // Parent placement must preserve the existing default child policy.
    assert_eq!(
        child_session.context_strategy,
        bubblepaw_core::ContextStrategy::default()
    );
    assert_eq!(
        child_session.parent_session_id.as_deref(),
        Some(fixture.owner.session_id.as_str())
    );
    assert_eq!(
        fixture.store.chat_path(child_id).unwrap(),
        fixture.store.chat_path(&fixture.owner.session_id).unwrap()
    );
    assert_eq!(
        child_session.title,
        format!(
            "Agent: answer using supplied context (parent {})",
            fixture.owner.session_id.chars().take(8).collect::<String>()
        )
    );
    assert!(
        fixture
            .store
            .list_sessions()
            .unwrap()
            .iter()
            .any(|session| session.id == child_id)
    );
    assert_eq!(
        fixture
            .store
            .outputs(&fixture.owner.session_id, 10)
            .unwrap()
            .len(),
        0
    );
    fixture.shutdown().await;
    let reopened = Store::open(fixture.root.path().join("archive.sqlite")).unwrap();
    let history = reopened.messages(child_id).unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(
        history.last().unwrap().content,
        result["text"].as_str().unwrap()
    );
    assert_eq!(reopened.runs(child_id).unwrap()[0].status, "completed");
}

#[tokio::test]
async fn child_js_is_restricted_outputs_stay_in_child_and_heaps_are_released() {
    let fixture = Fixture::new();
    // More jobs than CodeMode's sixteen total environment slots: cleanup is real.
    for _ in 0..20 {
        let started = fixture.spawn("inspect explicit file");
        let result = fixture.wait(&started).await;
        assert_eq!(result["status"], "completed");
        assert_eq!(result["text"], "child final answer");
        let child = started["sessionId"].as_str().unwrap();
        assert!(
            fixture
                .store
                .outputs(child, 10)
                .unwrap()
                .iter()
                .any(|output| output.output["text"] == "child-only output")
        );
        assert!(
            fixture.repl.status(child)["environments"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    assert!(
        fixture
            .store
            .outputs(&fixture.owner.session_id, 100)
            .unwrap()
            .is_empty()
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn timeout_does_not_cancel_ownership_is_checked_and_cancel_settles() {
    let fixture = Fixture::new();
    let started = fixture.spawn("block until cancelled");
    let handle = started["handle"].as_str().unwrap();
    let result = fixture
        .manager
        .wait(&fixture.owner.session_id, handle, Some(10))
        .await
        .unwrap();
    assert_eq!(result["status"], "timeout");
    assert_eq!(
        fixture
            .store
            .runs(started["sessionId"].as_str().unwrap())
            .unwrap()[0]
            .status,
        "running"
    );
    assert!(
        fixture
            .manager
            .wait("foreign", handle, Some(0))
            .await
            .is_err()
    );
    assert!(fixture.manager.cancel("foreign", handle).await.is_err());
    let cancelled = fixture
        .manager
        .cancel(&fixture.owner.session_id, handle)
        .await
        .unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(fixture.wait(&started).await, cancelled);
    assert!(
        fixture
            .manager
            .wait(&fixture.owner.session_id, "process:fake", Some(0))
            .await
            .is_err()
    );
    assert!(
        fixture
            .manager
            .wait(&fixture.owner.session_id, "agent:fake", Some(0))
            .await
            .is_err()
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn per_parent_and_host_limits_release_after_parent_cancel_and_shutdown() {
    let fixture = Fixture::new();
    let a = fixture.spawn("block one");
    let b = fixture.spawn("block two");
    assert!(
        fixture
            .manager
            .start(&fixture.owner, json!({"task":"block third"}))
            .is_err()
    );
    let second = fixture.store.create_session("second parent").unwrap();
    let second_context = ToolContext {
        session_id: second.id,
        ..fixture.owner.clone()
    };
    let c = fixture
        .manager
        .start(&second_context, json!({"task":"block third"}))
        .unwrap();
    let d = fixture
        .manager
        .start(&second_context, json!({"task":"block fourth"}))
        .unwrap();
    let third = fixture.store.create_session("third parent").unwrap();
    let third_context = ToolContext {
        session_id: third.id,
        ..fixture.owner.clone()
    };
    assert!(
        fixture
            .manager
            .start(&third_context, json!({"task":"blocked by host limit"}))
            .is_err()
    );
    assert_eq!(fixture.manager.cancel_session(&fixture.owner.session_id), 2);
    assert_eq!(fixture.wait(&a).await["status"], "cancelled");
    assert_eq!(fixture.wait(&b).await["status"], "cancelled");
    let next = fixture.spawn("normal task");
    assert_eq!(fixture.wait(&next).await["status"], "completed");
    fixture.shutdown().await;
    for child in [c, d] {
        assert_eq!(
            fixture
                .manager
                .wait(
                    &second_context.session_id,
                    child["handle"].as_str().unwrap(),
                    Some(0)
                )
                .await
                .unwrap()["status"],
            "cancelled"
        );
    }
    assert!(
        fixture
            .manager
            .start(&fixture.owner, json!({"task":"after shutdown"}))
            .is_err()
    );
}

#[tokio::test]
async fn failures_validation_and_cancellation_tokens_are_honest() {
    let fixture = Fixture::new();
    for input in [
        json!({"task":""}),
        json!({"task":3}),
        json!({"task":"x","extra":true}),
        json!({"task":"x","access":"workspace"}),
        json!({"task":"x","access":"root"}),
        json!({"task":"x","context":"x".repeat(64*1024)}),
    ] {
        assert!(fixture.manager.start(&fixture.owner, input).is_err());
    }
    let failed = fixture.spawn("failure task");
    let result = fixture.wait(&failed).await;
    assert_eq!(result["status"], "failed");
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("synthetic provider failure")
    );
    let blocked = fixture.spawn("block parent token");
    fixture.owner.cancellation.cancel();
    assert_eq!(fixture.wait(&blocked).await["status"], "cancelled");
    assert!(
        fixture
            .manager
            .start(&fixture.owner, json!({"task":"cancelled input"}))
            .is_err()
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn reply_is_bounded_even_with_json_escaping_and_full_answer_stays_canonical() {
    let fixture = Fixture::new();
    let started = fixture.spawn("large answer");
    let result = fixture.wait(&started).await;
    assert_eq!(result["status"], "completed");
    assert_eq!(result["resultTruncated"], true);
    assert!(result.to_string().len() <= 60 * 1024);
    let history = fixture
        .store
        .messages(started["sessionId"].as_str().unwrap())
        .unwrap();
    let canonical = &history.last().unwrap().content;
    assert_eq!(
        result["textBytes"].as_u64().unwrap(),
        canonical.len() as u64
    );
    assert!(canonical.len() > result["text"].as_str().unwrap().len());
    fixture.shutdown().await;
}

#[tokio::test]
async fn retained_handles_are_bounded_but_evicted_child_archive_remains() {
    let fixture = Fixture::new();
    let first = fixture.spawn("first retained child");
    fixture.wait(&first).await;
    for _ in 0..64 {
        let child = fixture.spawn("next retained child");
        assert_eq!(fixture.wait(&child).await["status"], "completed");
    }
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
    assert_eq!(
        fixture
            .store
            .messages(first["sessionId"].as_str().unwrap())
            .unwrap()
            .len(),
        2
    );
    fixture.shutdown().await;
}

#[path = "agents/workspace_access.rs"]
mod workspace_access;
