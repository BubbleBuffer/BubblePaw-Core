//! Scripted CodeMode and child-question delivery, without live providers.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, Message, ModelRequest, ModelResponse, Provider,
    Role, RunOptions, Store, TextCallback, ToolCall, ToolContext, ToolRegistry,
};
use bubblepaw_runner::{
    agent_handles::register_agent_handles,
    agents::{AgentProfile, AgentRegistry, AgentTools},
    artifacts::ArtifactStore,
    codemode::CodeMode,
    question::QuestionTools,
    wake::WakeCoordinator,
};
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

struct Script {
    store: Store,
    root: String,
    requests: Mutex<Vec<ModelRequest>>,
    parent_starts_child: bool,
}
#[async_trait]
impl Provider for Script {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let is_root = request.session_id == self.root;
        let has_result = request
            .messages
            .iter()
            .any(|message| message.tool_call_id.is_some());
        let needs_answer = is_root
            && request
                .messages
                .iter()
                .any(|message| message.content.contains("waiting_for_answer"))
            && !request
                .messages
                .iter()
                .any(|message| message.content.contains("\"answered\":true"));
        self.requests.lock().unwrap().push(request.clone());
        if has_result && !needs_answer {
            if !is_root {
                assert!(
                    request
                        .messages
                        .iter()
                        .any(|message| message.content.contains("Blue with stripes"))
                );
            }
            return Ok(ModelResponse {
                text: if is_root {
                    "parent answered"
                } else {
                    "child resumed"
                }
                .into(),
                ..Default::default()
            });
        }
        let code = if is_root {
            if self.parent_starts_child && !needs_answer {
                "const child = await tools.agent({task:'Pick a colour'}); return await wait({handle:child.handle})".into()
            } else {
                assert!(needs_answer);
                let child = self
                    .store
                    .list_sessions()?
                    .into_iter()
                    .find(|session| {
                        session.parent_session_id.as_deref() == Some(self.root.as_str())
                    })
                    .unwrap();
                format!(
                    "return await tools.agent_send({{sessionId:{},text:'Blue with stripes'}})",
                    json!(child.id)
                )
            }
        } else {
            "return await tools.question({text:'Which colour?',options:['Red','Blue']})".into()
        };
        Ok(ModelResponse {
            tool_calls: vec![ToolCall {
                id: uuid::Uuid::new_v4().to_string(),
                name: "js".into(),
                arguments: json!({"code":code}),
            }],
            ..Default::default()
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_question_wakes_idle_parent_and_agent_send_resumes_same_turn() -> Result<()> {
    child_question_case(true, false).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_question_wakes_when_optional_completion_wakes_are_disabled() -> Result<()> {
    child_question_case(false, false).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_question_returns_from_parent_wait_so_same_js_environment_can_answer() -> Result<()> {
    child_question_case(false, true).await
}
async fn child_question_case(optional_wakes: bool, parent_waits: bool) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let root_session = store.create_session("parent")?.id;
    let wake = Arc::new(WakeCoordinator::new(
        optional_wakes,
        Duration::from_millis(20),
    ));
    let questions = Arc::new(QuestionTools::new(store.clone(), wake.clone(), true));
    let repl = Arc::new(CodeMode::new(
        directory.path().into(),
        Arc::new(ArtifactStore::new(
            directory.path(),
            directory.path().join("artifacts"),
        )?),
    )?);
    let provider = Arc::new(Script {
        store: store.clone(),
        root: root_session.clone(),
        requests: Mutex::default(),
        parent_starts_child: parent_waits,
    });
    let mut child_tools = ToolRegistry::new();
    questions.register(&mut child_tools)?;
    repl.register(&mut child_tools)?;
    let child = Harness::new(
        store.clone(),
        provider.clone(),
        child_tools,
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    );
    let agents = Arc::new(
        AgentTools::from_profiles(
            store.clone(),
            vec![AgentProfile {
                name: "read-only".into(),
                description: "fixture".into(),
                harness: Arc::new(child),
                model: None,
                max_rounds: Some(12),
                approval: None,
            }],
        )?
        .with_registry(Arc::new(AgentRegistry::default().with_wake(wake.clone())))
        .with_repl(repl.clone()),
    );
    agents.set_questions(questions.clone())?;
    let mut tools = ToolRegistry::new();
    agents.register(&mut tools)?;
    register_agent_handles(&mut tools, agents.clone())?;
    questions.register(&mut tools)?;
    repl.register(&mut tools)?;
    let root = Arc::new(Harness::new(
        store.clone(),
        provider.clone(),
        tools,
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    ));
    let _registration = wake.register_harness(root.clone());
    let _driver = wake.start_driver();
    let context = ToolContext {
        session_id: root_session.clone(),
        run_id: "launch-fixture".into(),
        invocation_id: "launch".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    let child_id = if parent_waits {
        root.start(&root_session, "Delegate and wait", RunOptions::default())?;
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(child) = store.list_sessions()?.into_iter().find(|session| {
                    session.parent_session_id.as_deref() == Some(root_session.as_str())
                }) {
                    break Ok::<_, anyhow::Error>(child.id);
                }
                tokio::task::yield_now().await;
            }
        })
        .await??
    } else {
        agents.start(&context, json!({"task":"Pick a colour"}))?["sessionId"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let child_done = store
                .runs(&child_id)?
                .last()
                .is_some_and(|run| run.status == "completed");
            let parent_done = store
                .runs(&root_session)?
                .last()
                .is_some_and(|run| run.status == "completed");
            if child_done && parent_done {
                break Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await??;
    assert_eq!(
        store.runs(&child_id)?.len(),
        1,
        "answer should resume the waiting turn"
    );
    assert!(questions.pending(&child_id)?.is_none());
    assert_eq!(
        provider
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.session_id == child_id)
            .count(),
        2
    );
    assert!(store.pending_effects(&child_id)?.is_empty());
    agents.shutdown().await?;
    repl.shutdown().await;
    root.shutdown(Duration::from_secs(2)).await?;
    Ok(())
}

struct ResumeOnly(Mutex<Vec<ModelRequest>>);
#[async_trait]
impl Provider for ResumeOnly {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        assert!(
            request
                .messages
                .iter()
                .any(|message| message.content.contains("Answer: Keep the original"))
        );
        assert!(
            request
                .messages
                .iter()
                .any(|message| message.is_error && message.content.contains("outcome unknown"))
        );
        self.0.lock().unwrap().push(request);
        Ok(ModelResponse {
            text: "continued from durable answer".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn recovered_answer_is_accepted_once_without_replaying_unknown_effect() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let data = directory.path().join("recovery");
    let (session, chat) = {
        let store = Store::open_data_dir(&data)?;
        let session = store.create_session("interrupted question")?.id;
        store.append_batch(&[Message {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session.clone(),
            role: Role::Assistant,
            origin: None,
            content: String::new(),
            tool_calls: vec![
                ToolCall {
                    id: "unknown-call".into(),
                    name: "external_effect".into(),
                    arguments: json!({}),
                },
                ToolCall {
                    id: "old-js".into(),
                    name: "js".into(),
                    arguments: json!({"code":"return await tools.question({text:'Keep which?'})"}),
                },
            ],
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: "2026-10-15T00:00:00Z".into(),
        }])?;
        store.update_host_state(&session, "interrupted-run", "question", |_| Ok(json!({"id":"saved-question","status":"pending","text":"Keep which?","options":null,"run_id":"interrupted-run","parent_session_id":null})))?;
        let chat = store.chat_path(&session)?.expect("canonical question chat");
        (session, chat)
    };
    // Freeze the canonical boundary; no external operation or runtime is crashed.
    {
        let connection = rusqlite::Connection::open(&chat)?;
        let run = json!({"id":"interrupted-run","session_id":session,"model":"scripted","status":"running","text":null,"error":null,"started_at":"2026-10-15T00:00:00Z","ended_at":null,"usage":null});
        connection.execute(
            "INSERT INTO runs(id,session_id,status,data) VALUES('interrupted-run',?1,'running',?2)",
            rusqlite::params![session, run.to_string()],
        )?;
        connection.execute("INSERT INTO effects(id,session_id,run_id,call_id,state,detail) VALUES('unknown-call',?1,'interrupted-run','unknown-call','dispatched',?2)", rusqlite::params![session,json!({"id":"unknown-call","name":"external_effect","arguments":{}}).to_string()])?;
    }
    let store = Store::open_data_dir(&data)?;
    assert_eq!(store.pending_effects(&session)?[0]["state"], "unknown");
    let questions = Arc::new(QuestionTools::new(store.clone(), Arc::default(), true));
    let provider = Arc::new(ResumeOnly(Mutex::default()));
    let root = Arc::new(Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    ));
    let answer = questions.answer(&root, &session, "saved-question", "Keep the original")?;
    assert!(!answer.live);
    bubblepaw_runner::question::continue_answer(&root, &store, &session, &answer, None)?;
    root.wait_for_idle(&session).await?;
    let retry = questions.answer(&root, &session, "saved-question", "Keep the original")?;
    bubblepaw_runner::question::continue_answer(&root, &store, &session, &retry, None)?;
    assert_eq!(provider.0.lock().unwrap().len(), 1);
    assert_eq!(store.runs(&session)?.len(), 2);
    assert_eq!(store.pending_effects(&session)?[0]["state"], "unknown");
    root.shutdown(Duration::from_secs(2)).await?;
    Ok(())
}

struct NoticeProbe(Mutex<Vec<ModelRequest>>);
#[async_trait]
impl Provider for NoticeProbe {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.0.lock().unwrap().push(request);
        Ok(ModelResponse {
            text: "continued".into(),
            ..Default::default()
        })
    }
}

struct LiveAnswerProbe {
    requests: Mutex<Vec<ModelRequest>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    code: &'static str,
}
#[async_trait]
impl Provider for LiveAnswerProbe {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let has_result = request
            .messages
            .iter()
            .any(|message| message.tool_call_id.is_some());
        self.requests.lock().unwrap().push(request);
        if !has_result {
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: "live-question-cell".into(),
                    name: "js".into(),
                    arguments: json!({"code":self.code}),
                }],
                ..Default::default()
            });
        }
        self.entered.notify_one();
        self.release.notified().await;
        Ok(ModelResponse {
            text: "answer consumed".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn live_answer_notice_survives_pruned_run_metadata_without_reinjection() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let data = directory.path().join("live-answer");
    let (session, chat, run_id, marker) = {
        let store = Store::open_data_dir(&data)?;
        let session = store.create_session("live answer")?.id;
        let questions = Arc::new(QuestionTools::new(store.clone(), Arc::default(), true));
        let repl = Arc::new(CodeMode::new(
            directory.path().into(),
            Arc::new(ArtifactStore::new(
                directory.path(),
                directory.path().join("live-artifacts"),
            )?),
        )?);
        let provider = Arc::new(LiveAnswerProbe {
            requests: Mutex::default(),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            code: "return await tools.question({text:'Keep which?'})",
        });
        let mut tools = ToolRegistry::new();
        questions.register(&mut tools)?;
        repl.register(&mut tools)?;
        let root = Arc::new(Harness::new(
            store.clone(),
            provider.clone(),
            tools,
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                ..Default::default()
            },
        ));
        let run = root.start(&session, "Ask a question", RunOptions::default())?;
        let pending = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(question) = questions.pending(&session)? {
                    break Ok::<_, anyhow::Error>(question);
                }
                tokio::task::yield_now().await;
            }
        })
        .await??;
        let question_id = pending["id"].as_str().unwrap();
        let before_answer = store.storage_stats()?;
        let messages_before_completion = store.messages(&session)?.len();
        let outputs_before_completion = store.outputs(&session, 128)?.len();
        assert!(
            questions
                .answer(&root, &session, question_id, "Keep the original")?
                .live
        );
        assert_eq!(
            store.host_state(&session, "question")?.unwrap()["status"],
            "answered"
        );
        assert!(store.queued_inputs(&session)?.iter().any(|input| {
            matches!(input.state.as_str(), "pending" | "reminder_pending")
                && has_answer_delivery(&store, &session, &input.id, question_id)
        }));
        tokio::time::timeout(Duration::from_secs(3), provider.entered.notified()).await?;
        let with_receipt = store.storage_stats()?;
        let writes = with_receipt.chat_transaction_count - before_answer.chat_transaction_count;
        println!(
            "live question: answer state + accepted input + execution Output + JS ToolResult + input consumption={writes} transactions; WAL growth={} bytes",
            with_receipt
                .wal_bytes
                .saturating_sub(before_answer.wal_bytes)
        );
        assert_eq!(
            writes, 5,
            "two answer-acceptance transactions, one execution Output receipt, one JS ToolResult, and one accepted-input consumption transaction"
        );
        // CodeMode persists its execution completion before returning to the
        // harness (codemode.rs worker -> ToolDispatcher::emit_output). The
        // harness then stores the outer JS result and consumes accepted input.
        let outputs = store.outputs(&session, 128)?;
        assert_eq!(outputs.len() - outputs_before_completion, 1);
        assert!(outputs.iter().any(|output| output.run_id == run.id
            && output.output["type"] == "execution"
            && output.output["status"] == "completed"));
        let messages = store.messages(&session)?;
        let completed = &messages[messages_before_completion..];
        assert_eq!(completed.len(), 2);
        assert_eq!(completed[0].role, Role::Tool);
        assert_eq!(
            completed[0].tool_call_id.as_deref(),
            Some("live-question-cell")
        );
        assert_eq!(completed[1].role, Role::User);
        assert_eq!(
            completed[1].origin,
            Some(bubblepaw_core::MessageOrigin::Generated)
        );
        assert!(
            completed[1]
                .content
                .contains(&format!("<harness kind=\"question_answer:{question_id}\">"))
        );
        assert!(
            store
                .queued_inputs(&session)?
                .iter()
                .all(|input| input.state != "pending")
        );
        provider.release.notify_one();
        root.wait_for_idle(&session).await?;
        let marker = format!("<harness kind=\"question_answer:{question_id}\">");
        assert_eq!(
            store
                .messages(&session)?
                .iter()
                .filter(|message| message.content.contains(&marker))
                .count(),
            1
        );
        root.shutdown(Duration::from_secs(2)).await?;
        repl.shutdown().await;
        let chat = store
            .chat_path(&session)?
            .expect("canonical live question chat");
        (session, chat, run.id, marker)
    };
    // Remove disposable run metadata, keeping authoritative messages and state.
    for path in [chat, data.join("serving.db")] {
        let connection = rusqlite::Connection::open(path)?;
        connection.execute("DELETE FROM runs WHERE id=?1", [&run_id])?;
    }
    let store = Store::open_data_dir(&data)?;
    assert!(store.runs(&session)?.is_empty());
    let questions = Arc::new(QuestionTools::new(store.clone(), Arc::default(), true));
    let provider = Arc::new(NoticeProbe(Mutex::default()));
    let mut tools = ToolRegistry::new();
    questions.register(&mut tools)?;
    let before = store.storage_stats()?.transaction_count;
    assert!(
        tools
            .get_host("question")
            .unwrap()
            .take_reminders(&session)
            .is_empty()
    );
    assert_eq!(
        store.storage_stats()?.transaction_count,
        before,
        "unchanged notice checks must not write"
    );
    let root = Arc::new(Harness::new(
        store.clone(),
        provider.clone(),
        tools,
        HarnessConfig::default(),
    ));
    root.start(&session, "Next normal turn", RunOptions::default())?;
    root.wait_for_idle(&session).await?;
    assert_eq!(
        store
            .messages(&session)?
            .iter()
            .filter(|message| message.content.contains(&marker))
            .count(),
        1
    );
    root.shutdown(Duration::from_secs(2)).await?;
    Ok(())
}

#[path = "questions/sequential_questions.rs"]
mod sequential_questions;

#[path = "questions/delivery_cases.rs"]
mod delivery_cases;
use delivery_cases::has_answer_delivery;
