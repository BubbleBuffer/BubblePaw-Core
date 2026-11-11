//! Actual CodeMode question routing through recursive child/worker wake bindings.
//! Providers are scripted; optional completion wakes stay disabled in both cases.
use anyhow::{Result, bail};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, Role, Store,
    TextCallback, ToolCall, ToolContext, ToolRegistry,
};
use bubblepaw_runner::{
    agent_handles::{register_agent_handles, register_agent_handles_weak},
    agents::{AgentProfile, AgentRegistry, AgentTools},
    artifacts::ArtifactStore,
    codemode::CodeMode,
    config::Agents,
    question::QuestionTools,
};
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Notify;

const QUESTION: &str = "Which answer belongs to my immediate parent?";
const ANSWER: &str = "Answered by the immediate parent";

struct RoutingScript {
    store: Store,
    root: String,
    requests: Mutex<Vec<ModelRequest>>,
    counts: Mutex<HashMap<String, usize>>,
    leaf_can_ask: Notify,
    parent_woke: Notify,
    parent_can_answer: Notify,
}
#[async_trait]
impl Provider for RoutingScript {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let session = self.store.session(&request.session_id)?;
        self.requests.lock().unwrap().push(request.clone());
        let turn = {
            let mut counts = self.counts.lock().unwrap();
            let count = counts.entry(session.id.clone()).or_default();
            *count += 1;
            *count
        };
        if session.id == self.root {
            bail!("a descendant question escaped its direct parent");
        }
        let direct_child = session.parent_session_id.as_deref() == Some(self.root.as_str());
        let code = if direct_child {
            match turn {
                1 => Some("return await tools.agent({task:'leaf-question'})".into()),
                2 => None,
                3 => {
                    let leaf = self
                        .store
                        .list_sessions()?
                        .into_iter()
                        .find(|child| {
                            child.parent_session_id.as_deref() == Some(session.id.as_str())
                        })
                        .expect("parent launched its own child");
                    assert!(request.messages.iter().any(|message| message.origin
                        == Some(bubblepaw_core::MessageOrigin::Generated)
                        && message.content.contains("waiting_for_answer")
                        && message.content.contains(&leaf.id)
                        && message.content.contains(QUESTION)));
                    self.parent_woke.notify_one();
                    self.parent_can_answer.notified().await;
                    Some(format!(
                        "return await tools.agent_send({{sessionId:{},text:{}}})",
                        json!(leaf.id),
                        json!(ANSWER)
                    ))
                }
                4 => {
                    assert!(
                        request
                            .messages
                            .iter()
                            .any(|message| message.tool_call_id.is_some()
                                && message.content.contains("\"answered\":true"))
                    );
                    None
                }
                _ => bail!("unexpected extra parent request {turn}"),
            }
        } else {
            match turn {
                1 => {
                    self.leaf_can_ask.notified().await;
                    Some(format!(
                        "return await tools.question({{text:{},options:['parent answer']}})",
                        json!(QUESTION)
                    ))
                }
                2 => {
                    assert!(
                        request
                            .messages
                            .iter()
                            .any(|message| message.role == Role::Tool
                                && message.content.contains(ANSWER))
                    );
                    None
                }
                _ => bail!("unexpected extra leaf request {turn}"),
            }
        };
        Ok(match code {
            Some(code) => ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "js".into(),
                    arguments: json!({"code":code}),
                }],
                ..Default::default()
            },
            None => ModelResponse {
                text: if direct_child {
                    "parent finished"
                } else {
                    "leaf finished"
                }
                .into(),
                ..Default::default()
            },
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grandchild_question_wakes_only_its_idle_direct_parent() -> Result<()> {
    routing_case(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hidden_workers_ordinary_child_question_wakes_its_idle_parent() -> Result<()> {
    routing_case(true).await
}

async fn routing_case(hidden_worker: bool) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let root_id = store.create_session("question ancestor")?.id;
    let repl = Arc::new(CodeMode::new(
        directory.path().into(),
        Arc::new(ArtifactStore::new(
            directory.path(),
            directory.path().join("artifacts"),
        )?),
    )?);
    let script = Arc::new(RoutingScript {
        store: store.clone(),
        root: root_id.clone(),
        requests: Mutex::default(),
        counts: Mutex::default(),
        leaf_can_ask: Notify::new(),
        parent_woke: Notify::new(),
        parent_can_answer: Notify::new(),
    });
    let agents = AgentTools::new_recursive(
        store.clone(),
        Agents {
            auto_wake: false,
            wake_debounce_ms: 5,
            ..Default::default()
        },
        Arc::new(AgentRegistry::default()),
        Some(repl.clone()),
        None,
        |manager| {
            let mut tools = ToolRegistry::new();
            manager.register_weak(&mut tools)?;
            register_agent_handles_weak(&mut tools, manager)?;
            // QuestionTools shares this exact registry/wake host at every depth.
            let questions = Arc::new(QuestionTools::new(
                store.clone(),
                manager.wake_coordinator(),
                true,
            ));
            manager.set_questions(questions.clone())?;
            questions.register(&mut tools)?;
            repl.register(&mut tools)?;
            Ok(vec![AgentProfile {
                name: "read-only".into(),
                description: "recursive question fixture".into(),
                model: None,
                max_rounds: Some(12),
                approval: None,
                harness: Arc::new(Harness::new(
                    store.clone(),
                    script.clone(),
                    tools,
                    HarnessConfig {
                        execution_mode: bubblepaw_core::ExecutionMode::Js,
                        ..Default::default()
                    },
                )),
            }])
        },
    )?;
    let wake = agents.wake_coordinator();
    let mut root_tools = ToolRegistry::new();
    agents.register(&mut root_tools)?;
    register_agent_handles(&mut root_tools, agents.clone())?;
    repl.register(&mut root_tools)?;
    let root = Arc::new(Harness::new(
        store.clone(),
        script.clone(),
        root_tools,
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    ));
    let _registration = wake.register_harness(root.clone());
    let _driver = wake.start_driver();
    let root_context = ToolContext {
        session_id: root_id.clone(),
        run_id: "fixture-launch".into(),
        invocation_id: "fixture-launch".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    let parent = if hidden_worker {
        let mut profile = agents.profile("read-only").unwrap().clone();
        profile.name = "internal-question-parent".into();
        agents.start_worker(&root_context, profile, json!({"task":"parent-task"}))?
    } else {
        agents.start(&root_context, json!({"task":"parent-task"}))?
    };
    let parent_id = parent["sessionId"].as_str().unwrap();
    assert_eq!(
        agents.wait(&root_id, parent_id, Some(3_000)).await?["status"],
        "completed"
    );
    let leaf = store
        .list_sessions()?
        .into_iter()
        .find(|session| session.parent_session_id.as_deref() == Some(parent_id))
        .expect("nested child exists");
    assert_eq!(store.session(parent_id)?.hidden, hidden_worker);
    assert_eq!(leaf.hidden, hidden_worker);
    assert!(store.runs(&root_id)?.is_empty());
    assert_eq!(
        store.runs(parent_id)?.len(),
        1,
        "parent must be idle before its child asks"
    );
    script.leaf_can_ask.notify_one();
    tokio::time::timeout(Duration::from_secs(3), script.parent_woke.notified()).await?;
    assert_eq!(
        store.runs(parent_id)?.len(),
        2,
        "question must admit a fresh turn on the immediate parent"
    );
    assert!(
        store.runs(&root_id)?.is_empty(),
        "question must not wake the ancestor"
    );
    assert!(
        agents
            .send(
                &root_context,
                json!({"sessionId":leaf.id,"text":"ancestor cannot answer this"})
            )
            .is_err()
    );
    let state = agents.status(parent_id, &json!({"sessionId":leaf.id}))?;
    assert_eq!(state["status"], "waiting_for_answer");
    assert_eq!(state["question"]["text"], QUESTION);
    script.parent_can_answer.notify_one();
    assert_eq!(
        agents.wait(&root_id, parent_id, Some(3_000)).await?["status"],
        "completed"
    );
    assert_eq!(
        agents.wait(parent_id, &leaf.id, Some(3_000)).await?["status"],
        "completed"
    );
    assert_eq!(
        store.runs(&leaf.id)?.len(),
        1,
        "answer must resume the original descendant turn"
    );
    assert!(store.runs(&root_id)?.is_empty());
    assert!(
        store
            .messages(&root_id)?
            .iter()
            .all(|message| !message.content.contains(QUESTION))
    );
    assert!(wake.take_reminders(&root_id, "question:").is_empty());
    if hidden_worker {
        assert!(
            agents.take_reminders(&root_id).is_empty(),
            "hidden worker remains silent outward"
        );
    }
    {
        let requests = script.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.session_id == parent_id)
                .count(),
            4
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.session_id == leaf.id)
                .count(),
            2
        );
    }
    agents.shutdown().await?;
    repl.shutdown().await;
    root.shutdown(Duration::from_secs(2)).await?;
    Ok(())
}
