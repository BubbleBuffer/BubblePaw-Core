use super::modes::{AUTO_ARGUMENT_BYTES, bounded_arguments};
use super::*;
use crate::{
    CancellationToken, HarnessConfig, HarnessEvent, ModelRequest, ModelResponse, Provider,
    RunOptions, TextCallback, Tool, ToolDefinition, ToolDispatcher, ToolRegistry, ToolResult,
};
use async_trait::async_trait;
use std::collections::VecDeque;
use tokio::sync::broadcast;

/// Replays responses, then answers "done" so a session can take more runs.
struct Script(Mutex<VecDeque<ModelResponse>>);
impl Script {
    fn new(responses: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(responses.into())))
    }
}
#[async_trait]
impl Provider for Script {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(self.0.lock().unwrap().pop_front().unwrap_or(ModelResponse {
            text: "done".into(),
            ..Default::default()
        }))
    }
}

struct Effect {
    name: &'static str,
    approval: Approval,
    calls: AtomicUsize,
}
fn effect(name: &'static str, approval: Approval) -> Arc<Effect> {
    Arc::new(Effect {
        name,
        approval,
        calls: AtomicUsize::new(0),
    })
}

#[test]
fn archived_decisions_keep_terminal_errors_and_cancellation_is_a_noop() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let session = store.create_session("approval archive")?;
    let record = ApprovalRecord {
        id: crate::id(),
        session_id: session.id.clone(),
        run_id: "synthetic-run".into(),
        call_id: "synthetic-call".into(),
        tool: "write".into(),
        arguments: serde_json::json!({"synthetic":true}),
        state: ApprovalState::Pending,
        reason: None,
        created_at: now(),
        decided_at: None,
    };
    insert(&store, &record)?;
    let approved = decide(&store, &record.id, Decision::Approve)?;
    assert_eq!(approved.state, ApprovalState::Approved);
    let chat = store.scoped(&session.id)?.unwrap();
    assert_eq!(
        chat.read(|conn| Ok(conn
            .query_row("SELECT COUNT(*) FROM approvals", [], |row| row
                .get::<_, i64>(0))?))?,
        0
    );
    let error = decide(
        &store,
        &record.id,
        Decision::Deny {
            reason: Some("late".into()),
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("already approved"), "{error}");
    assert!(
        transition(
            &store,
            &record.id,
            ApprovalState::Cancelled,
            Some("late cancellation")
        )?
        .is_none()
    );
    assert_eq!(load(&store, &record.id)?.state, ApprovalState::Approved);
    Ok(())
}
#[async_trait]
impl Tool for Effect {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: "effect".into(),
            input_schema: serde_json::json!({"type":"object"}),
            read_only: false,
            approval: self.approval,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text("effect ran"))
    }
}

/// A model entrypoint composing one host call, like a native composition wrapper.
struct Bridge;
#[async_trait]
impl Tool for Bridge {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "compose".into(),
            description: "bridge".into(),
            input_schema: serde_json::json!({"type":"object"}),
            read_only: false,
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
        dispatcher
            .invoke(
                context,
                input["target"].as_str().unwrap(),
                serde_json::json!({}),
            )
            .await
    }
}

fn direct(call_id: &str, tool: &str) -> ModelResponse {
    ModelResponse {
        tool_calls: vec![ToolCall {
            id: call_id.into(),
            name: tool.into(),
            arguments: serde_json::json!({"path":"a.txt"}),
        }],
        ..Default::default()
    }
}
fn nested(call_id: &str, target: &str) -> ModelResponse {
    ModelResponse {
        tool_calls: vec![ToolCall {
            id: call_id.into(),
            name: "compose".into(),
            arguments: serde_json::json!({"target":target}),
        }],
        ..Default::default()
    }
}
fn harness(
    store: &Store,
    provider: Arc<Script>,
    tools: ToolRegistry,
    approval: ApprovalPolicy,
) -> Harness {
    Harness::new(
        store.clone(),
        provider,
        tools,
        HarnessConfig {
            execution_mode: crate::ExecutionMode::Native,
            approval,
            ..Default::default()
        },
    )
}
fn direct_tools(tool: &Arc<Effect>) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).unwrap();
    tools
}
async fn requested(events: &mut broadcast::Receiver<HarnessEvent>) -> ApprovalRecord {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let EventKind::ApprovalRequested { approval } = events.recv().await.unwrap().kind {
                return approval;
            }
        }
    })
    .await
    .expect("approval requested")
}
fn drain(events: &mut broadcast::Receiver<HarnessEvent>) -> Vec<EventKind> {
    std::iter::from_fn(|| events.try_recv().ok().map(|event| event.kind)).collect()
}
fn effects(store: &Store) -> i64 {
    store
        .read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM effects", [], |r| r.get(0))?))
        .unwrap()
}
fn approvals(store: &Store) -> Vec<(String, Option<String>)> {
    store
        .read(|conn| {
            let mut statement =
                conn.prepare("SELECT state,reason FROM approvals ORDER BY rowid")?;
            Ok(statement
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .unwrap()
}
fn tool_message(store: &Store, session: &str, call_id: &str) -> crate::Message {
    store
        .messages(session)
        .unwrap()
        .into_iter()
        .find(|m| m.tool_call_id.as_deref() == Some(call_id))
        .expect("tool result message")
}
fn started(kinds: &[EventKind], call_id: &str) -> bool {
    kinds
        .iter()
        .any(|k| matches!(k, EventKind::ToolStarted { call } if call.id == call_id))
}
fn finished(kinds: &[EventKind], call_id: &str) -> bool {
    kinds
        .iter()
        .any(|k| matches!(k, EventKind::ToolFinished { call_id: id, .. } if id == call_id))
}

#[test]
fn policy_order_and_denial_text() {
    let mut policy = ApprovalPolicy::default();
    assert_eq!(policy.mode("plain", Approval::None), ApprovalMode::Allow);
    assert_eq!(policy.mode("write", Approval::Confirm), ApprovalMode::Ask);
    policy.confirm = ApprovalMode::Deny;
    assert_eq!(policy.mode("write", Approval::Confirm), ApprovalMode::Deny);
    policy.tools.insert("write".into(), ApprovalMode::Allow);
    policy.tools.insert("plain".into(), ApprovalMode::Ask);
    assert_eq!(policy.mode("write", Approval::Confirm), ApprovalMode::Allow);
    assert_eq!(policy.mode("plain", Approval::None), ApprovalMode::Ask);
    assert_eq!(
        denial(None),
        "Denied by the user. Do not retry this call unless the user asks."
    );
    assert_eq!(
        denial(Some("not now")),
        "Denied by the user: not now. Do not retry this call unless the user asks."
    );
    let wire: ApprovalPolicy =
        serde_json::from_value(serde_json::json!({"tools":{"exec":"ask"}})).unwrap();
    assert_eq!(wire.confirm, ApprovalMode::Ask);
    assert_eq!(wire.tools["exec"], ApprovalMode::Ask);
    assert_eq!(
        Decision::parse("deny", Some("x".into())).unwrap(),
        Decision::Deny {
            reason: Some("x".into())
        }
    );
    assert!(Decision::parse("maybe", None).is_err());
}

enum AutoResult {
    Probability(f64),
    Error,
    Waiting,
}
struct Approver {
    result: AutoResult,
    requests: Mutex<Vec<crate::DecisionRequest>>,
    entered: tokio::sync::Notify,
}
fn auto_usage() -> crate::Usage {
    crate::Usage {
        input_tokens: 23,
        output_tokens: 5,
        cache_read_tokens: 11,
        cache_creation_tokens: 3,
        reasoning_tokens: None,
    }
}
#[async_trait]
impl crate::DecisionProvider for Approver {
    async fn decide(
        &self,
        request: crate::DecisionRequest,
        _: CancellationToken,
    ) -> Result<crate::DecisionResponse> {
        self.requests.lock().unwrap().push(request);
        self.entered.notify_one();
        let probability = match self.result {
            AutoResult::Probability(p) => p,
            AutoResult::Error => {
                return Err(crate::ProviderError::new(
                    crate::ProviderErrorKind::Fatal,
                    "approver unavailable",
                )
                .with_usage(auto_usage())
                .into());
            }
            AutoResult::Waiting => std::future::pending().await,
        };
        Ok(crate::DecisionResponse {
            answers: vec![crate::Answer {
                name: "approve".into(),
                kind: crate::AnswerKind::Predicate { probability },
            }],
            model: "decision/test-model".into(),
            usage: auto_usage().into(),
        })
    }
}
fn auto_harness(
    store: &Store,
    tools: ToolRegistry,
    response: ModelResponse,
    result: AutoResult,
) -> (Harness, Arc<Approver>) {
    let approver = Arc::new(Approver {
        result,
        requests: Mutex::new(Vec::new()),
        entered: Default::default(),
    });
    let harness = Harness::new(
        store.clone(),
        Script::new(vec![response]),
        tools,
        HarnessConfig {
            execution_mode: crate::ExecutionMode::Native,
            approval: ApprovalPolicy {
                confirm: ApprovalMode::Auto,
                ..Default::default()
            },
            auto_approval: Some(AutoApprovalConfig {
                provider: approver.clone(),
                model: "approver".into(),
                prompt: crate::DEFAULT_AUTO_PROMPT.into(),
                working_directory: "C:\\project".into(),
                resolve: None,
            }),
            ..Default::default()
        },
    );
    (harness, approver)
}

#[tokio::test]
async fn auto_usage_shares_the_existing_approval_transaction() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("approval billing boundary")?;
    let tool = effect("write", Approval::Confirm);
    let (_, approver) = auto_harness(
        &store,
        direct_tools(&tool),
        direct("c1", "write"),
        AutoResult::Probability(0.95),
    );
    let automatic = AutoApprovalContext {
        config: AutoApprovalConfig {
            provider: approver,
            model: "configured/approver".into(),
            prompt: crate::DEFAULT_AUTO_PROMPT.into(),
            working_directory: "project".into(),
            resolve: None,
        },
        user_request: "write the file".into(),
    };
    let context = ToolContext {
        session_id: session.id.clone(),
        run_id: "run-for-approval".into(),
        invocation_id: String::new(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    let response = direct("call-for-approval", "write");
    let before = store.storage_stats()?.transaction_count;
    let result = gate(
        &store,
        &ApprovalPolicy {
            confirm: ApprovalMode::Auto,
            ..Default::default()
        },
        Some(&automatic),
        &context,
        &[],
        CallSource {
            call: &response.tool_calls[0],
            message_id: None,
            interactive: true,
        },
        &tool.definition(),
    )
    .await?;
    assert!(matches!(result, Gate::Proceed));
    assert_eq!(
        store.storage_stats()?.transaction_count - before,
        1,
        "decider billing shares the already required approval insert"
    );
    let rows = store.request_usage()?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].usage, auto_usage());
    assert_eq!(rows[0].kind, crate::RequestKind::Decider);
    assert_eq!(rows[0].session_id, session.id);
    assert_eq!(rows[0].run_id.as_deref(), Some(context.run_id.as_str()));
    assert!(!rows[0].failed);
    assert_eq!(approvals(&store)[0].0, "approved");
    Ok(())
}

struct AppendUser {
    store: Store,
    session: String,
    script: Arc<Script>,
}
#[async_trait]
impl Provider for AppendUser {
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        self.store.append(&Store::message(
            &self.session,
            crate::Role::User,
            "a generated message added during the model call".into(),
        ))?;
        self.script.generate(request, on_text, cancellation).await
    }
}

#[tokio::test]
async fn auto_uses_consumed_run_request_before_provider_appends_other_users() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("request scope")?;
    store.append(&Store::message(
        &session.id,
        crate::Role::User,
        "prior request".into(),
    ))?;
    let tool = effect("write", Approval::Confirm);
    let approver = Arc::new(Approver {
        result: AutoResult::Probability(0.9),
        requests: Mutex::new(Vec::new()),
        entered: Default::default(),
    });
    let agent = Harness::new(
        store.clone(),
        Arc::new(AppendUser {
            store: store.clone(),
            session: session.id.clone(),
            script: Script::new(vec![direct("c1", "write")]),
        }),
        direct_tools(&tool),
        HarnessConfig {
            approval: ApprovalPolicy {
                confirm: ApprovalMode::Auto,
                ..Default::default()
            },
            auto_approval: Some(AutoApprovalConfig {
                provider: approver.clone(),
                model: "approver".into(),
                prompt: "asked {user_request}; {unknown}".into(),
                working_directory: "project".into(),
                resolve: None,
            }),
            ..Default::default()
        },
    );
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(10),
            agent.prompt(&session.id, "current run request", RunOptions::default(),)
        )
        .await??
        .status,
        "completed"
    );
    let requests = approver.requests.lock().unwrap();
    assert_eq!(requests[0].input, "asked current run request; {unknown}");
    assert_eq!(
        tool.calls.load(Ordering::SeqCst),
        1,
        "a yes answer approves"
    );
    Ok(())
}

#[tokio::test]
async fn auto_yes_approves_without_waiting_and_records_reason() -> Result<()> {
    for nested_call in [false, true] {
        let store = Store::in_memory()?;
        let session = store.create_session("auto")?;
        let tool = effect("write", Approval::Confirm);
        let mut tools = ToolRegistry::new();
        let response = if nested_call {
            tools.register(Arc::new(Bridge))?;
            tools.register_host(tool.clone())?;
            nested("c1", "write")
        } else {
            tools.register(tool.clone())?;
            direct("c1", "write")
        };
        let (agent, approver) =
            auto_harness(&store, tools, response, AutoResult::Probability(0.95));
        let mut events = agent.subscribe();
        let run = tokio::time::timeout(
            Duration::from_secs(10),
            agent.prompt(
                &session.id,
                "write the requested file",
                RunOptions::default(),
            ),
        )
        .await??;
        assert_eq!(run.status, "completed");
        assert_eq!(
            run.usage,
            Some(crate::Usage::default()),
            "approver usage does not alter main run totals"
        );
        let usage: Vec<_> = store
            .request_usage()?
            .into_iter()
            .filter(|record| record.kind == crate::RequestKind::Decider)
            .collect();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].usage, auto_usage());
        assert_eq!(usage[0].provider, "decision");
        assert_eq!(usage[0].model, "test-model");
        assert_eq!(usage[0].run_id.as_deref(), Some(run.id.as_str()));
        assert!(!usage[0].failed);
        assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            approvals(&store),
            [(
                "approved".into(),
                Some("auto decision/test-model p=0.95".into())
            )]
        );
        assert!(agent.pending_approvals(&session.id)?.is_empty());
        let kinds = drain(&mut events);
        let approval_events: Vec<_> = kinds
            .iter()
            .filter(|kind| {
                matches!(
                    kind,
                    EventKind::ApprovalRequested { .. } | EventKind::ApprovalResolved { .. }
                )
            })
            .collect();
        assert!(
            matches!(approval_events[0], EventKind::ApprovalRequested { approval }
            if approval.state == ApprovalState::Approved && approval.decided_at.is_some())
        );
        assert!(matches!(approval_events[1], EventKind::ApprovalResolved {
            state: ApprovalState::Approved, reason: Some(reason), ..
        } if reason == "auto decision/test-model p=0.95"));
        let requests = approver.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "one decision, no repeated samples");
        let input = &requests[0].input;
        for field in ["write the requested file", "Tool: write", "effect"] {
            assert!(input.contains(field), "{field} missing from {input}");
        }
        assert!(input.contains("C:\\project"), "{input}");
        assert_eq!(requests[0].questions.len(), 1);
        assert_eq!(requests[0].session_id.as_deref(), Some(session.id.as_str()));
    }
    Ok(())
}

#[tokio::test]
async fn auto_resolver_updates_model_and_prompt_at_the_next_gate() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("live approver")?;
    let tool = effect("write", Approval::Confirm);
    let approver = Arc::new(Approver {
        result: AutoResult::Probability(0.8),
        requests: Mutex::default(),
        entered: Default::default(),
    });
    let changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let resolve = {
        let approver = approver.clone();
        let changed = changed.clone();
        Arc::new(move || {
            let current = changed.load(Ordering::SeqCst);
            AutoApprovalConfig {
                provider: approver.clone(),
                model: if current {
                    "new-approver"
                } else {
                    "old-approver"
                }
                .into(),
                prompt: if current { "new {tool}" } else { "old {tool}" }.into(),
                working_directory: "project".into(),
                resolve: None,
            }
        })
    };
    let mut automatic = resolve();
    automatic.resolve = Some(resolve);
    let agent = Harness::new(
        store.clone(),
        Script::new(vec![
            direct("first", "write"),
            ModelResponse {
                text: "done".into(),
                ..Default::default()
            },
            direct("second", "write"),
        ]),
        direct_tools(&tool),
        HarnessConfig {
            approval: ApprovalPolicy {
                confirm: ApprovalMode::Auto,
                ..Default::default()
            },
            auto_approval: Some(automatic),
            ..Default::default()
        },
    );
    agent
        .prompt(&session.id, "first gate", RunOptions::default())
        .await?;
    changed.store(true, Ordering::SeqCst);
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(10),
            agent.prompt(&session.id, "next gate", RunOptions::default())
        )
        .await??
        .status,
        "completed"
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 2);
    let requests = approver.requests.lock().unwrap();
    assert_eq!(requests[0].model, "old-approver");
    assert_eq!(requests[1].model, "new-approver");
    assert_eq!(requests[0].input, "old write");
    assert_eq!(requests[1].input, "new write");
    Ok(())
}

#[tokio::test]
async fn auto_no_or_provider_error_waits_for_a_person() -> Result<()> {
    for result in [
        AutoResult::Probability(0.5),
        AutoResult::Error,
        AutoResult::Probability(f64::NAN),
    ] {
        let store = Store::in_memory()?;
        let session = store.create_session("auto ask")?;
        let tool = effect("write", Approval::Confirm);
        let failed = matches!(&result, AutoResult::Error);
        let (agent, _) = auto_harness(&store, direct_tools(&tool), direct("c1", "write"), result);
        let mut events = agent.subscribe();
        agent.start(&session.id, "go", RunOptions::default())?;
        let approval = requested(&mut events).await;
        assert_eq!(approval.state, ApprovalState::Pending);
        assert_eq!(approval.reason, None);
        assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
        assert_eq!(effects(&store), 0);
        let usage: Vec<_> = store
            .request_usage()?
            .into_iter()
            .filter(|record| record.kind == crate::RequestKind::Decider)
            .collect();
        assert_eq!(
            usage.len(),
            1,
            "decider usage commits before the human wait"
        );
        assert_eq!(usage[0].usage, auto_usage());
        assert_eq!(usage[0].failed, failed);
        agent.decide(&approval.id, Decision::Approve)?;
        assert_eq!(agent.wait_for_idle(&session.id).await?.status, "completed");
        assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    }
    Ok(())
}

#[tokio::test]
async fn cancellation_during_auto_call_falls_through_to_cancelled_wait() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("cancel auto")?;
    let tool = effect("write", Approval::Confirm);
    let (agent, approver) = auto_harness(
        &store,
        direct_tools(&tool),
        direct("c1", "write"),
        AutoResult::Waiting,
    );
    let mut events = agent.subscribe();
    agent.start(&session.id, "go", RunOptions::default())?;
    tokio::time::timeout(Duration::from_secs(10), approver.entered.notified()).await?;
    assert!(agent.cancel(&session.id));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), agent.wait_for_idle(&session.id))
            .await??
            .status,
        "cancelled"
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    assert_eq!(effects(&store), 0);
    assert_eq!(approvals(&store), [("cancelled".into(), None)]);
    assert!(agent.pending_approvals(&session.id)?.is_empty());
    let usage: Vec<_> = store
        .request_usage()?
        .into_iter()
        .filter(|record| record.kind == crate::RequestKind::Decider)
        .collect();
    assert_eq!(usage.len(), 1);
    assert!(usage[0].failed);
    assert_eq!(usage[0].usage, crate::Usage::default());
    let kinds = drain(&mut events);
    assert!(kinds.iter().any(
        |kind| matches!(kind, EventKind::ApprovalRequested { approval }
        if approval.state == ApprovalState::Pending)
    ));
    assert!(kinds.iter().any(|kind| matches!(
        kind,
        EventKind::ApprovalResolved {
            state: ApprovalState::Cancelled,
            ..
        }
    )));
    Ok(())
}

#[test]
fn auto_arguments_are_bounded_on_utf8_boundaries() {
    let bounded = bounded_arguments(&serde_json::json!({"content": "😀".repeat(4096)}));
    assert!(bounded.len() <= AUTO_ARGUMENT_BYTES + "\n[arguments truncated]".len());
    assert!(bounded.ends_with("[arguments truncated]"));
    let policy: ApprovalPolicy = serde_json::from_value(serde_json::json!({
        "confirm": "auto", "tools": {"exec": "auto"}
    }))
    .unwrap();
    assert_eq!(policy.mode("write", Approval::Confirm), ApprovalMode::Auto);
    assert_eq!(policy.mode("exec", Approval::None), ApprovalMode::Auto);
}

#[tokio::test]
async fn direct_call_waits_for_approval_then_runs() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("approve")?;
    let tool = effect("write", Approval::Confirm);
    let agent = harness(
        &store,
        Script::new(vec![direct("c1", "write")]),
        direct_tools(&tool),
        ApprovalPolicy::default(),
    );
    let mut events = agent.subscribe();
    agent.start(&session.id, "go", RunOptions::default())?;
    let approval = requested(&mut events).await;
    assert_eq!(
        (
            approval.state,
            approval.tool.as_str(),
            approval.call_id.as_str()
        ),
        (ApprovalState::Pending, "write", "c1")
    );
    assert_eq!(approval.arguments["path"], "a.txt");
    assert_eq!(agent.pending_approvals(&session.id)?.len(), 1);
    assert_eq!(effects(&store), 0, "nothing is dispatched before approval");
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    assert!(!started(&drain(&mut events), "c1"));
    let decided = agent.decide(&approval.id, Decision::Approve)?;
    assert_eq!(decided.state, ApprovalState::Approved);
    assert!(decided.decided_at.is_some());
    assert_eq!(agent.wait_for_idle(&session.id).await?.status, "completed");
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    assert_eq!(effects(&store), 1);
    assert!(agent.pending_approvals(&session.id)?.is_empty());
    let kinds = drain(&mut events);
    assert!(matches!(
        kinds.first(),
        Some(EventKind::ApprovalResolved {
            state: ApprovalState::Approved,
            ..
        })
    ));
    assert!(started(&kinds, "c1"));
    let error = agent.decide(&approval.id, Decision::Approve).unwrap_err();
    assert!(error.to_string().contains("already approved"), "{error}");
    let error = agent.decide("missing", Decision::Approve).unwrap_err();
    assert!(error.to_string().contains("Unknown approval"), "{error}");
    Ok(())
}

#[tokio::test]
async fn direct_denial_is_an_error_result_without_effect() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("deny")?;
    let tool = effect("write", Approval::Confirm);
    let agent = harness(
        &store,
        Script::new(vec![direct("c1", "write")]),
        direct_tools(&tool),
        ApprovalPolicy::default(),
    );
    let mut events = agent.subscribe();
    agent.start(&session.id, "go", RunOptions::default())?;
    let approval = requested(&mut events).await;
    agent.decide(
        &approval.id,
        Decision::Deny {
            reason: Some("not now".into()),
        },
    )?;
    assert_eq!(agent.wait_for_idle(&session.id).await?.status, "completed");
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    assert_eq!(effects(&store), 0);
    let message = tool_message(&store, &session.id, "c1");
    assert!(message.is_error);
    assert_eq!(
        message.content,
        "Denied by the user: not now. Do not retry this call unless the user asks."
    );
    let kinds = drain(&mut events);
    assert!(!started(&kinds, "c1"));
    assert!(finished(&kinds, "c1"), "same path as a rejected call");
    assert_eq!(
        approvals(&store),
        [("denied".into(), Some("not now".into()))]
    );
    Ok(())
}

#[tokio::test]
async fn nested_call_waits_and_denial_rejects_the_host_call() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("nested")?;
    let tool = effect("effect", Approval::Confirm);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Bridge))?;
    tools.register_host(tool.clone())?;
    let agent = harness(
        &store,
        Script::new(vec![nested("n1", "effect"), nested("n2", "effect")]),
        tools,
        ApprovalPolicy::default(),
    );
    let mut events = agent.subscribe();
    agent.start(&session.id, "go", RunOptions::default())?;
    let first = requested(&mut events).await;
    assert_eq!(first.tool, "effect");
    assert_ne!(first.call_id, "n1", "nested calls have their own ids");
    assert_eq!(effects(&store), 1, "only the outer entrypoint so far");
    agent.decide(&first.id, Decision::Approve)?;
    let second = requested(&mut events).await;
    agent.decide(&second.id, Decision::Deny { reason: None })?;
    assert_eq!(agent.wait_for_idle(&session.id).await?.status, "completed");
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        effects(&store),
        3,
        "two entrypoints and one approved effect"
    );
    assert!(!tool_message(&store, &session.id, "n1").is_error);
    let denied = tool_message(&store, &session.id, "n2");
    assert!(denied.is_error);
    assert_eq!(
        denied.content,
        "Denied by the user. Do not retry this call unless the user asks."
    );
    Ok(())
}

#[tokio::test]
async fn approve_for_session_covers_later_runs_of_that_session_only() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("rule")?;
    let other = store.create_session("other")?;
    let tool = effect("write", Approval::Confirm);
    let text = |text: &str| ModelResponse {
        text: text.into(),
        ..Default::default()
    };
    let agent = harness(
        &store,
        Script::new(vec![
            direct("c1", "write"),
            text("first"),
            direct("c2", "write"),
            text("second"),
            direct("c3", "write"),
        ]),
        direct_tools(&tool),
        ApprovalPolicy::default(),
    );
    let mut events = agent.subscribe();
    agent.start(&session.id, "go", RunOptions::default())?;
    let approval = requested(&mut events).await;
    assert_eq!(
        agent
            .decide(&approval.id, Decision::ApproveForSession)?
            .state,
        ApprovalState::ApprovedSession
    );
    agent.wait_for_idle(&session.id).await?;
    drain(&mut events);
    let run = agent
        .prompt(&session.id, "again", RunOptions::default())
        .await?;
    assert_eq!(run.status, "completed");
    assert_eq!(tool.calls.load(Ordering::SeqCst), 2);
    assert!(
        !drain(&mut events)
            .iter()
            .any(|k| matches!(k, EventKind::ApprovalRequested { .. }))
    );
    agent.start(&other.id, "elsewhere", RunOptions::default())?;
    let elsewhere = requested(&mut events).await;
    assert_eq!(elsewhere.session_id, other.id);
    agent.decide(&elsewhere.id, Decision::Deny { reason: None })?;
    agent.wait_for_idle(&other.id).await?;
    assert_eq!(tool.calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn policy_allow_deny_and_overrides_need_no_person() -> Result<()> {
    let policy = |confirm, tools: &[(&str, ApprovalMode)]| ApprovalPolicy {
        confirm,
        tools: tools.iter().map(|(n, m)| (n.to_string(), *m)).collect(),
        timeout_secs: None,
        ..Default::default()
    };
    let confirm = Approval::Confirm;
    // (harness policy, run override, tool, declared, tool runs, stored record)
    let cases = [
        (
            policy(ApprovalMode::Allow, &[]),
            None,
            "write",
            confirm,
            true,
            None,
        ),
        (
            policy(ApprovalMode::Ask, &[("write", ApprovalMode::Allow)]),
            None,
            "write",
            confirm,
            true,
            None,
        ),
        (
            policy(ApprovalMode::Allow, &[("write", ApprovalMode::Deny)]),
            None,
            "write",
            confirm,
            false,
            Some("denied"),
        ),
        (
            policy(ApprovalMode::Deny, &[]),
            None,
            "plain",
            Approval::None,
            true,
            None,
        ),
        (
            ApprovalPolicy::default(),
            Some(policy(ApprovalMode::Deny, &[])),
            "write",
            confirm,
            false,
            Some("denied"),
        ),
        (
            policy(ApprovalMode::Deny, &[]),
            Some(policy(ApprovalMode::Allow, &[])),
            "write",
            confirm,
            true,
            None,
        ),
    ];
    for (index, (config, run_policy, name, declared, runs, record)) in cases.into_iter().enumerate()
    {
        let store = Store::in_memory()?;
        let session = store.create_session("policy")?;
        let tool = effect(name, declared);
        let agent = harness(
            &store,
            Script::new(vec![direct("c1", name)]),
            direct_tools(&tool),
            config,
        );
        let options = RunOptions {
            approval: run_policy,
            ..Default::default()
        };
        let run = tokio::time::timeout(
            Duration::from_secs(10),
            agent.prompt(&session.id, "go", options),
        )
        .await
        .expect("no wait for a person")?;
        assert_eq!(run.status, "completed", "case {index}");
        assert_eq!(
            tool.calls.load(Ordering::SeqCst),
            usize::from(runs),
            "case {index}"
        );
        let stored = approvals(&store);
        match record {
            None => assert!(stored.is_empty(), "case {index}"),
            Some(state) => {
                assert_eq!(
                    stored,
                    [(state.into(), Some("policy".into()))],
                    "case {index}"
                );
                assert_eq!(
                    tool_message(&store, &session.id, "c1").content,
                    "Denied by the user: policy. Do not retry this call unless the user asks."
                );
                assert_eq!(effects(&store), 0);
            }
        }
    }
    // A tool-specific Ask applies to tools that declare nothing.
    let store = Store::in_memory()?;
    let session = store.create_session("ask plain")?;
    let tool = effect("plain", Approval::None);
    let agent = harness(
        &store,
        Script::new(vec![direct("c1", "plain")]),
        direct_tools(&tool),
        policy(ApprovalMode::Allow, &[("plain", ApprovalMode::Ask)]),
    );
    let mut events = agent.subscribe();
    agent.start(&session.id, "go", RunOptions::default())?;
    let approval = requested(&mut events).await;
    agent.decide(&approval.id, Decision::Approve)?;
    agent.wait_for_idle(&session.id).await?;
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn timeout_expires_the_approval_and_denies_the_call() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("timeout")?;
    let tool = effect("write", Approval::Confirm);
    let agent = harness(
        &store,
        Script::new(vec![direct("c1", "write")]),
        direct_tools(&tool),
        ApprovalPolicy {
            timeout_secs: Some(1),
            ..Default::default()
        },
    );
    let mut events = agent.subscribe();
    agent.start(&session.id, "go", RunOptions::default())?;
    let approval = requested(&mut events).await;
    assert_eq!(agent.wait_for_idle(&session.id).await?.status, "completed");
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    assert_eq!(effects(&store), 0);
    assert_eq!(
        approvals(&store),
        [("expired".into(), Some("timeout".into()))]
    );
    let message = tool_message(&store, &session.id, "c1");
    assert!(message.is_error);
    assert_eq!(message.content, "Approval timed out.");
    assert!(drain(&mut events).iter().any(|k| matches!(
        k,
        EventKind::ApprovalResolved {
            state: ApprovalState::Expired,
            ..
        }
    )));
    let error = agent.decide(&approval.id, Decision::Approve).unwrap_err();
    assert!(error.to_string().contains("already expired"), "{error}");
    Ok(())
}

#[tokio::test]
async fn cancel_while_pending_cancels_the_approval_and_the_run() -> Result<()> {
    for nested_call in [false, true] {
        let store = Store::in_memory()?;
        let session = store.create_session("cancel")?;
        let tool = effect("write", Approval::Confirm);
        let mut tools = ToolRegistry::new();
        let script = if nested_call {
            tools.register(Arc::new(Bridge))?;
            tools.register_host(tool.clone())?;
            Script::new(vec![nested("c1", "write")])
        } else {
            tools.register(tool.clone())?;
            Script::new(vec![direct("c1", "write")])
        };
        let agent = harness(&store, script, tools, ApprovalPolicy::default());
        let mut events = agent.subscribe();
        agent.start(&session.id, "go", RunOptions::default())?;
        let approval = requested(&mut events).await;
        assert!(agent.cancel(&session.id));
        assert_eq!(agent.wait_for_idle(&session.id).await?.status, "cancelled");
        assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            approvals(&store),
            [("cancelled".into(), None)],
            "nested={nested_call}"
        );
        assert!(agent.pending_approvals(&session.id)?.is_empty());
        assert!(drain(&mut events).iter().any(|k| matches!(
            k,
            EventKind::ApprovalResolved {
                state: ApprovalState::Cancelled,
                ..
            }
        )));
        assert!(agent.decide(&approval.id, Decision::Approve).is_err());
        assert!(
            store
                .pending_effects(&session.id)?
                .iter()
                .all(|e| e["call"].is_null()),
            "the gated call never crossed the effect boundary"
        );
    }
    Ok(())
}

#[test]
fn restart_expires_pending_approvals_and_interrupts_their_run() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("archive.sqlite");
    let (session, run_id, approval_id) = {
        let store = Store::open(&path)?;
        let session = store.create_session("restart")?;
        let run = crate::Run {
            id: id(),
            session_id: session.id.clone(),
            model: "default".into(),
            status: "running".into(),
            text: None,
            error: None,
            started_at: now(),
            ended_at: None,
            usage: None,
        };
        store.admit(&run, Some("go"), crate::MessageOrigin::Typed)?;
        let record = ApprovalRecord {
            id: id(),
            session_id: session.id.clone(),
            run_id: run.id.clone(),
            call_id: "c1".into(),
            tool: "write".into(),
            arguments: serde_json::json!({}),
            state: ApprovalState::Pending,
            reason: None,
            created_at: now(),
            decided_at: None,
        };
        insert(&store, &record)?;
        (session.id, run.id, record.id)
    };
    let store = Store::open(&path)?;
    assert_eq!(store.run(&run_id)?.status, "interrupted");
    assert!(pending(&store, &session)?.is_empty());
    let record = load(&store, &approval_id)?;
    assert_eq!(record.state, ApprovalState::Expired);
    assert_eq!(record.reason.as_deref(), Some("restart"));
    assert!(decide(&store, &approval_id, Decision::Approve).is_err());
    Ok(())
}

#[test]
fn deny_rules_use_exact_session_rule_grammar_and_union_inherited_rules() {
    let parent = ApprovalPolicy {
        deny: vec!["exec".into(), "mcp__server__erase".into()],
        ..Default::default()
    };
    let policy = ApprovalPolicy {
        deny: vec!["apply_patch".into(), "exec".into(), "mcp__*".into()],
        ..Default::default()
    }
    .with_inherited_denies(&parent);
    assert_eq!(
        policy.deny,
        ["apply_patch", "exec", "mcp__*", "mcp__server__erase"]
    );
    assert_eq!(policy.denied_rule("exec"), Some("exec"));
    assert_eq!(
        policy.denied_rule("mcp__server__erase"),
        Some("mcp__server__erase")
    );
    assert_eq!(policy.denied_rule("execute"), None);
    assert_eq!(policy.denied_rule("mcp__server__read"), None);
    assert_eq!(policy.denied_rule("Exec"), None);
}

#[tokio::test]
async fn configured_deny_precedes_modes_session_allow_and_run_overrides() -> Result<()> {
    for nested_call in [false, true] {
        for mode in [
            ApprovalMode::Ask,
            ApprovalMode::Auto,
            ApprovalMode::Allow,
            ApprovalMode::Deny,
        ] {
            for run_override in [false, true] {
                let store = Store::in_memory()?;
                let session = store.create_session("deny rule")?;
                store.write(|tx| {
                    tx.execute(
                        "INSERT INTO approval_rules(session_id,tool) VALUES(?1,'write')",
                        [&session.id],
                    )?;
                    Ok(())
                })?;
                let tool = effect("write", Approval::Confirm);
                let mut tools = ToolRegistry::new();
                let response = if nested_call {
                    tools.register(Arc::new(Bridge))?;
                    tools.register_host(tool.clone())?;
                    nested("c1", "write")
                } else {
                    tools.register(tool.clone())?;
                    direct("c1", "write")
                };
                let agent = harness(
                    &store,
                    Script::new(vec![response]),
                    tools,
                    ApprovalPolicy {
                        confirm: mode,
                        tools: [("write".into(), mode)].into(),
                        deny: vec!["write".into()],
                        ..Default::default()
                    },
                );
                let mut events = agent.subscribe();
                let run = tokio::time::timeout(
                    Duration::from_secs(10),
                    agent.prompt(
                        &session.id,
                        "go",
                        RunOptions {
                            approval: run_override.then(|| ApprovalPolicy {
                                confirm: ApprovalMode::Allow,
                                tools: [("write".into(), ApprovalMode::Allow)].into(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    ),
                )
                .await
                .expect("deny must never wait for a person")?;
                assert_eq!(run.status, "completed");
                assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
                assert_eq!(
                    approvals(&store),
                    [("denied".into(), Some("deny rule write".into()))]
                );
                let kinds = drain(&mut events);
                assert!(!kinds.iter().any(|kind| matches!(kind,
                    EventKind::ToolStarted { call } if call.name == "write"
                )));
                assert!(kinds.iter().any(|kind| matches!(kind,
                    EventKind::ApprovalRequested { approval } if approval.state == ApprovalState::Denied
                )));
                assert!(
                    tool_message(&store, &session.id, "c1")
                        .content
                        .contains("deny rule write")
                );
                if !nested_call {
                    assert_eq!(effects(&store), 0);
                }
            }
        }
    }
    Ok(())
}

/// A single nested call can change the host policy before its next dispatch.
struct DenyFlip(Arc<Mutex<ApprovalPolicy>>);
#[async_trait]
impl Tool for DenyFlip {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "flip_deny".into(),
            description: "scripted policy reload".into(),
            input_schema: serde_json::json!({"type":"object"}),
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
        _: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        let first = dispatcher
            .invoke(context.clone(), "write", serde_json::json!({}))
            .await?;
        assert!(!first.is_error);
        self.0.lock().unwrap().deny.push("write".into());
        let blocked = dispatcher
            .invoke(context.clone(), "write", serde_json::json!({}))
            .await?;
        assert!(blocked.is_error);
        assert!(blocked.content.contains("deny rule write"));
        self.0.lock().unwrap().deny.clear();
        let allowed = dispatcher
            .invoke(context, "write", serde_json::json!({}))
            .await?;
        assert!(!allowed.is_error);
        Ok(ToolResult::text("updated deny rules applied"))
    }
}

#[tokio::test]
async fn live_deny_add_remove_applies_inside_a_run_override() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("live deny override")?;
    let current = Arc::new(Mutex::new(ApprovalPolicy::default()));
    let tool = effect("write", Approval::Confirm);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(DenyFlip(current.clone())))?;
    tools.register_host(tool.clone())?;
    let agent = Harness::new(
        store.clone(),
        Script::new(vec![direct("c1", "flip_deny")]),
        tools,
        HarnessConfig {
            approval_resolver: Some(Arc::new(move |_| current.lock().unwrap().clone())),
            ..Default::default()
        },
    );
    let run = agent
        .prompt(
            &session.id,
            "go",
            RunOptions {
                approval: Some(ApprovalPolicy {
                    confirm: ApprovalMode::Allow,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(run.status, "completed");
    assert!(!tool_message(&store, &session.id, "c1").is_error);
    assert_eq!(tool.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        approvals(&store),
        [("denied".into(), Some("deny rule write".into()))]
    );
    Ok(())
}

#[path = "approvals_tests_hardline.rs"]
mod hardline_tests;
