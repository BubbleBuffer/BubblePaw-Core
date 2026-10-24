use super::*;
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

#[path = "live_tests_registry.rs"]
mod registry;

struct Script {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}
impl Script {
    fn new(responses: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::default(),
        })
    }
}
#[async_trait]
impl Provider for Script {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        Ok(self.responses.lock().unwrap().pop_front().unwrap())
    }
}
fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: serde_json::json!({}),
    }
}
fn calls(calls: Vec<ToolCall>) -> ModelResponse {
    ModelResponse {
        tool_calls: calls,
        ..Default::default()
    }
}
fn done() -> ModelResponse {
    ModelResponse {
        text: "done".into(),
        ..Default::default()
    }
}
struct ValueTool {
    name: &'static str,
    value: &'static str,
    calls: Arc<AtomicUsize>,
    approval: Approval,
}
#[async_trait]
impl Tool for ValueTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: self.value.into(),
            input_schema: serde_json::json!({"type":"object"}),
            approval: self.approval,
            read_only: self.approval == Approval::None,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text(self.value))
    }
}
fn value(name: &'static str, text: &'static str) -> Arc<ValueTool> {
    Arc::new(ValueTool {
        name,
        value: text,
        calls: Arc::default(),
        approval: Approval::None,
    })
}
struct PolicyFlip {
    deny: Arc<AtomicBool>,
    nested: bool,
}
#[async_trait]
impl Tool for PolicyFlip {
    fn definition(&self) -> ToolDefinition {
        value("flip", "change policy").definition()
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
        if self.nested {
            assert!(
                !dispatcher
                    .invoke(context.clone(), "effect", serde_json::json!({}))
                    .await?
                    .is_error
            );
        }
        self.deny.store(true, Ordering::SeqCst);
        if self.nested {
            let result = dispatcher
                .invoke(context, "effect", serde_json::json!({}))
                .await?;
            assert!(result.is_error);
            return Ok(result);
        }
        Ok(ToolResult::text("policy changed"))
    }
}
fn policy(mode: ApprovalMode) -> ApprovalPolicy {
    ApprovalPolicy {
        confirm: mode,
        ..Default::default()
    }
}

#[tokio::test]
async fn approval_updates_apply_to_next_direct_and_nested_call() -> Result<()> {
    for nested in [false, true] {
        let store = Store::in_memory()?;
        let session = store.create_session("live approval")?;
        let deny = Arc::new(AtomicBool::new(false));
        let effect = Arc::new(ValueTool {
            name: "effect",
            value: "effect",
            calls: Arc::default(),
            approval: Approval::Confirm,
        });
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(PolicyFlip {
            deny: deny.clone(),
            nested,
        }))?;
        let response = if nested {
            tools.register_host(effect.clone())?;
            calls(vec![call("flip", "flip")])
        } else {
            tools.register(effect.clone())?;
            calls(vec![
                call("first", "effect"),
                call("flip", "flip"),
                call("second", "effect"),
            ])
        };
        let harness = Harness::new(
            store.clone(),
            Script::new(vec![response, done()]),
            tools,
            HarnessConfig {
                approval_resolver: Some(Arc::new(move |_| {
                    policy(if deny.load(Ordering::SeqCst) {
                        ApprovalMode::Deny
                    } else {
                        ApprovalMode::Allow
                    })
                })),
                ..Default::default()
            },
        );
        let run = harness
            .prompt(&session.id, "go", RunOptions::default())
            .await?;
        assert_eq!(run.status, "completed");
        assert_eq!(effect.calls.load(Ordering::SeqCst), 1);
        assert!(
            store
                .messages(&session.id)?
                .iter()
                .any(|message| message.role == Role::Tool
                    && message.content.contains("Denied by the user: policy"))
        );
    }
    Ok(())
}

#[tokio::test]
async fn live_approval_is_not_bypassed_by_single_call_and_run_override_stays_pinned() -> Result<()>
{
    for pinned in [false, true] {
        let store = Store::in_memory()?;
        let session = store.create_session("single approval")?;
        let effect = Arc::new(ValueTool {
            name: "effect",
            value: "effect",
            calls: Arc::default(),
            approval: Approval::Confirm,
        });
        let mut tools = ToolRegistry::new();
        tools.register(effect.clone())?;
        let harness = Harness::new(
            store,
            Script::new(vec![calls(vec![call("one", "effect")]), done()]),
            tools,
            HarnessConfig {
                approval: policy(ApprovalMode::Allow),
                approval_resolver: Some(Arc::new(|_| policy(ApprovalMode::Deny))),
                ..Default::default()
            },
        );
        let run = harness
            .prompt(
                &session.id,
                "go",
                RunOptions {
                    approval: pinned.then(|| policy(ApprovalMode::Allow)),
                    ..Default::default()
                },
            )
            .await?;
        assert_eq!(run.status, "completed");
        assert_eq!(effect.calls.load(Ordering::SeqCst), usize::from(pinned));
    }
    Ok(())
}

#[test]
fn inherited_approval_resolver_keeps_origin_session_and_pinned_policy() -> Result<()> {
    let deny = Arc::new(AtomicBool::new(false));
    let state = deny.clone();
    let dispatcher = ToolDispatcher::new(Store::in_memory()?, ToolRegistry::new(), 1024, None)
        .with_approval(policy(ApprovalMode::Deny))
        .with_approval_resolver(Some(Arc::new(move |session| {
            assert_eq!(session, "parent");
            policy(if state.load(Ordering::SeqCst) {
                ApprovalMode::Deny
            } else {
                ApprovalMode::Allow
            })
        })));
    let inherited = dispatcher.approval_resolver("parent");
    assert_eq!(inherited("child").confirm, ApprovalMode::Allow);
    deny.store(true, Ordering::SeqCst);
    assert_eq!(inherited("child").confirm, ApprovalMode::Deny);
    let pinned = dispatcher
        .with_approval_resolver(None)
        .approval_resolver("parent");
    deny.store(false, Ordering::SeqCst);
    assert_eq!(pinned("child").confirm, ApprovalMode::Deny);
    Ok(())
}

#[tokio::test]
async fn live_allow_policy_keeps_single_call_at_four_transactions() -> Result<()> {
    let mut transactions = Vec::new();
    for live in [false, true] {
        let store = Store::in_memory()?;
        let session = store.create_session("admission transaction count")?;
        let effect = Arc::new(ValueTool {
            name: "effect",
            value: "effect",
            calls: Arc::default(),
            approval: Approval::Confirm,
        });
        let mut tools = ToolRegistry::new();
        tools.register(effect.clone())?;
        let resolve: ApprovalResolver = Arc::new(|_| policy(ApprovalMode::Allow));
        let harness = Harness::new(
            store.clone(),
            Script::new(vec![calls(vec![call("effect", "effect")]), done()]),
            tools,
            HarnessConfig {
                approval: policy(ApprovalMode::Allow),
                approval_resolver: live.then_some(resolve),
                ..Default::default()
            },
        );
        let before = store.storage_stats()?.transaction_count;
        let run = harness
            .prompt(&session.id, "go", RunOptions::default())
            .await?;
        assert_eq!(run.status, "completed");
        assert_eq!(effect.calls.load(Ordering::SeqCst), 1);
        transactions.push(store.storage_stats()?.transaction_count - before);
    }
    // Input admission, combined assistant/dispatch, tool result, final answer.
    assert_eq!(
        transactions,
        [4, 4],
        "static and live policy must batch identically"
    );
    Ok(())
}

struct AdmittedPolicyProbe;
#[async_trait]
impl Tool for AdmittedPolicyProbe {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            approval: Approval::Confirm,
            read_only: false,
            ..value("probe", "probe admitted policy").definition()
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
        assert_eq!(context.approval.confirm, ApprovalMode::Allow);
        let nested = dispatcher
            .invoke(context, "effect", serde_json::json!({}))
            .await?;
        assert!(nested.is_error);
        assert!(nested.content.contains("Denied by the user: policy"));
        Ok(ToolResult::text("admission captured; nested call denied"))
    }
}

#[tokio::test]
async fn admitted_direct_policy_is_resolved_once_and_nested_call_gets_new_policy() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("admitted policy snapshot")?;
    let effect = Arc::new(ValueTool {
        name: "effect",
        value: "must not run",
        calls: Arc::default(),
        approval: Approval::Confirm,
    });
    let resolutions = Arc::new(AtomicUsize::new(0));
    let resolved = resolutions.clone();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(AdmittedPolicyProbe))?;
    tools.register_host(effect.clone())?;
    let harness = Harness::new(
        store,
        Script::new(vec![calls(vec![call("probe", "probe")]), done()]),
        tools,
        HarnessConfig {
            approval_resolver: Some(Arc::new(move |_| {
                policy(if resolved.fetch_add(1, Ordering::SeqCst) == 0 {
                    ApprovalMode::Allow
                } else {
                    ApprovalMode::Deny
                })
            })),
            ..Default::default()
        },
    );
    let run = harness
        .prompt(&session.id, "go", RunOptions::default())
        .await?;
    assert_eq!(run.status, "completed");
    assert_eq!(effect.calls.load(Ordering::SeqCst), 0);
    assert_eq!(resolutions.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn removed_tool_reasons_are_unadvertised_and_cleared_when_registered_again() -> Result<()> {
    let mut tools = ToolRegistry::new();
    tools.register_host(value("removed", "old"))?;
    tools.remove("removed", "server config removed");
    assert!(tools.capabilities().is_empty());
    assert!(tools.get("removed").is_none());
    assert!(
        tools
            .validate_model_call(&call("gone", "removed"))
            .unwrap_err()
            .to_string()
            .contains("server config removed")
    );
    let dispatcher = ToolDispatcher::new(Store::in_memory()?, tools.clone(), 1024, None);
    let error = dispatcher
        .invoke(
            ToolContext {
                session_id: "session".into(),
                run_id: "run".into(),
                invocation_id: String::new(),
                cancellation: CancellationToken::new(),
                approval: ApprovalPolicy::default(),
            },
            "removed",
            serde_json::json!({}),
        )
        .await?;
    assert!(error.is_error);
    assert_eq!(
        error.content,
        "Tool removed is unavailable: server config removed"
    );
    tools.register_host(value("removed", "new"))?;
    assert_eq!(tools.capabilities().len(), 1);
    assert!(tools.validate("removed", &serde_json::json!({})).is_ok());
    Ok(())
}
