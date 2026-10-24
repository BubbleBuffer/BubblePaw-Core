//! Hardline policy across direct and native composition routes.
use super::*;

/// A model call to `exec` with a command string, as a direct shell call.
fn exec_direct(call_id: &str, command: &str) -> ModelResponse {
    ModelResponse {
        tool_calls: vec![ToolCall {
            id: call_id.into(),
            name: "exec".into(),
            arguments: serde_json::json!({"command": command}),
        }],
        ..Default::default()
    }
}
/// A model call to `compose` that runs `exec` with the command inside the cell.
fn exec_bridge(call_id: &str, command: &str) -> ModelResponse {
    ModelResponse {
        tool_calls: vec![ToolCall {
            id: call_id.into(),
            name: "compose".into(),
            arguments: serde_json::json!({"target": "exec", "command": command}),
        }],
        ..Default::default()
    }
}
/// Forwards a `compose` call's command to a host `exec`, like codemode's `tools.exec`.
struct CommandBridge;
#[async_trait]
impl Tool for CommandBridge {
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
        let command = input["command"].clone();
        dispatcher
            .invoke(
                context,
                input["target"].as_str().unwrap(),
                serde_json::json!({"command": command}),
            )
            .await
    }
}

const RM_ROOT: &str = "hardline rm -rf of the filesystem root";

#[tokio::test]
async fn hardline_precedes_every_mode_session_allowance_and_run_override() -> Result<()> {
    // Fake `exec`: the command is never run, only counted if it reaches the tool.
    for nested_call in [false, true] {
        for mode in [
            ApprovalMode::Ask,
            ApprovalMode::Auto,
            ApprovalMode::Allow,
            ApprovalMode::Deny,
        ] {
            for allowance in [false, true] {
                for run_override in [false, true] {
                    let store = Store::in_memory()?;
                    let session = store.create_session("hardline")?;
                    if allowance {
                        store.write(|tx| {
                            tx.execute(
                                "INSERT INTO approval_rules(session_id,tool) VALUES(?1,'exec')",
                                [&session.id],
                            )?;
                            Ok(())
                        })?;
                    }
                    let tool = effect("exec", Approval::Confirm);
                    let mut tools = ToolRegistry::new();
                    let response = if nested_call {
                        tools.register(Arc::new(CommandBridge))?;
                        tools.register_host(tool.clone())?;
                        exec_bridge("c1", "rm -rf /")
                    } else {
                        tools.register(tool.clone())?;
                        exec_direct("c1", "rm -rf /")
                    };
                    let agent = harness(
                        &store,
                        Script::new(vec![response]),
                        tools,
                        ApprovalPolicy {
                            confirm: mode,
                            tools: [("exec".into(), mode)].into(),
                            ..Default::default()
                        },
                    );
                    let run = tokio::time::timeout(
                        Duration::from_secs(10),
                        agent.prompt(
                            &session.id,
                            "go",
                            RunOptions {
                                approval: run_override.then(|| ApprovalPolicy {
                                    confirm: ApprovalMode::Allow,
                                    tools: [("exec".into(), ApprovalMode::Allow)].into(),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            },
                        ),
                    )
                    .await
                    .expect("hardline must never wait for a person")?;
                    let case = format!(
                        "nested={nested_call} mode={mode:?} allowance={allowance} override={run_override}"
                    );
                    assert_eq!(run.status, "completed", "{case}");
                    assert_eq!(tool.calls.load(Ordering::SeqCst), 0, "{case}");
                    assert_eq!(
                        approvals(&store),
                        [("denied".into(), Some(RM_ROOT.into()))],
                        "{case}"
                    );
                    assert!(
                        tool_message(&store, &session.id, "c1")
                            .content
                            .contains("Blocked by hardline rule rm -rf of the filesystem root"),
                        "{case}"
                    );
                }
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn commands_off_the_list_run_under_allow_and_the_floor_can_be_off() -> Result<()> {
    for command in [
        "git push --force-with-lease origin main",
        "git status --short",
        "rm -rf ./target",
    ] {
        let store = Store::in_memory()?;
        let session = store.create_session("hardline allowed")?;
        let tool = effect("exec", Approval::Confirm);
        let agent = harness(
            &store,
            Script::new(vec![exec_direct("c1", command)]),
            direct_tools(&tool),
            ApprovalPolicy {
                confirm: ApprovalMode::Allow,
                ..Default::default()
            },
        );
        let run = agent
            .prompt(&session.id, "go", RunOptions::default())
            .await?;
        assert_eq!(run.status, "completed");
        assert_eq!(tool.calls.load(Ordering::SeqCst), 1, "{command}");
        assert!(approvals(&store).is_empty(), "{command}");
    }
    // With the floor off, the same blocked command runs under Allow.
    let store = Store::in_memory()?;
    let session = store.create_session("hardline off")?;
    let tool = effect("exec", Approval::Confirm);
    let agent = harness(
        &store,
        Script::new(vec![exec_direct("c1", "rm -rf /")]),
        direct_tools(&tool),
        ApprovalPolicy {
            confirm: ApprovalMode::Allow,
            hardline: false,
            ..Default::default()
        },
    );
    let run = agent
        .prompt(&session.id, "go", RunOptions::default())
        .await?;
    assert_eq!(run.status, "completed");
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    assert!(approvals(&store).is_empty());
    Ok(())
}

#[tokio::test]
async fn run_override_cannot_turn_off_the_host_hardline_floor() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("hardline override")?;
    let tool = effect("exec", Approval::Confirm);
    let agent = harness(
        &store,
        Script::new(vec![exec_direct("c1", "rm -rf /")]),
        direct_tools(&tool),
        ApprovalPolicy::default(),
    );
    let run = tokio::time::timeout(
        Duration::from_secs(10),
        agent.prompt(
            &session.id,
            "go",
            RunOptions {
                approval: Some(ApprovalPolicy {
                    confirm: ApprovalMode::Allow,
                    hardline: false,
                    ..Default::default()
                }),
                ..Default::default()
            },
        ),
    )
    .await
    .expect("hardline must never wait for a person")?;
    assert_eq!(run.status, "completed");
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    assert_eq!(approvals(&store), [("denied".into(), Some(RM_ROOT.into()))]);
    Ok(())
}

/// Blocks and then runs `exec` through the dispatcher, flipping the host's
/// hardline setting between calls, as a live config reload would.
struct HardlineFlip(Arc<Mutex<ApprovalPolicy>>);
#[async_trait]
impl Tool for HardlineFlip {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "flip_hardline".into(),
            description: "scripted hardline reload".into(),
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
        let call = serde_json::json!({"command": "rm -rf /"});
        let blocked = dispatcher
            .invoke(context.clone(), "exec", call.clone())
            .await?;
        assert!(blocked.is_error);
        self.0.lock().unwrap().hardline = false;
        let ran = dispatcher
            .invoke(context.clone(), "exec", call.clone())
            .await?;
        assert!(!ran.is_error);
        self.0.lock().unwrap().hardline = true;
        let blocked_again = dispatcher.invoke(context, "exec", call).await?;
        assert!(blocked_again.is_error);
        Ok(ToolResult::text("live hardline toggle applied"))
    }
}

#[tokio::test]
async fn live_hardline_toggle_applies_to_the_next_exec_call() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("live hardline")?;
    let host = Arc::new(Mutex::new(ApprovalPolicy {
        confirm: ApprovalMode::Allow,
        ..Default::default()
    }));
    let tool = effect("exec", Approval::Confirm);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HardlineFlip(host.clone())))?;
    tools.register_host(tool.clone())?;
    let agent = Harness::new(
        store.clone(),
        Script::new(vec![direct("c1", "flip_hardline")]),
        tools,
        HarnessConfig {
            approval_resolver: Some(Arc::new(move |_| host.lock().unwrap().clone())),
            ..Default::default()
        },
    );
    let run = agent
        .prompt(&session.id, "go", RunOptions::default())
        .await?;
    assert_eq!(run.status, "completed");
    assert!(!tool_message(&store, &session.id, "c1").is_error);
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        approvals(&store),
        [
            ("denied".into(), Some(RM_ROOT.into())),
            ("denied".into(), Some(RM_ROOT.into())),
        ]
    );
    Ok(())
}
