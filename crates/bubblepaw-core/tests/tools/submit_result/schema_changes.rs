//! The chat's declaration is immutable; the active run's result validation is live.
use super::*;

struct JsRoute;
#[async_trait]
impl Tool for JsRoute {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "js".into(),
            description: "Scripted JS access wrapper".into(),
            input_schema: json!({"type":"object","properties":{"code":{"type":"string"}},"required":["code"]}),
            read_only: true,
            approval: Approval::None,
        }
    }
    fn definition_in(&self, registry: &ToolRegistry) -> ToolDefinition {
        ToolDefinition {
            description: serde_json::to_string(&registry.capabilities()).unwrap(),
            ..self.definition()
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
        let target: Value = serde_json::from_str(input["code"].as_str().unwrap())?;
        dispatcher
            .invoke(
                context,
                target["name"].as_str().unwrap(),
                target["arguments"].clone(),
            )
            .await
    }
}

fn changed_options() -> RunOptions {
    RunOptions {
        output_schema: Some(
            JsonSchema::new(
                "changed_answer",
                json!({
                    "type":"object","properties":{"answer":{"type":"string","minLength":1}},
                    "required":["answer"],"additionalProperties":false
                }),
            )
            .unwrap(),
        ),
        tool_names: Some(vec![]),
        ..Default::default()
    }
}
fn result_call(mode: ExecutionMode, wrapped: bool, arguments: Value) -> ModelResponse {
    match mode {
        ExecutionMode::Js => batch(vec![call(
            "js",
            json!({"code":json!({"name":"submit_result","arguments":arguments}).to_string()}),
        )]),
        ExecutionMode::Native if wrapped => batch(vec![call(
            "call_tool",
            json!({"name":"submit_result","arguments":arguments}),
        )]),
        ExecutionMode::Native => submit(arguments),
    }
}
fn prefix(request: &ModelRequest) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(
        &json!({"system":request.system_prompt,"tools":request.tools}),
    )?)
}
fn schema_notice(request: &ModelRequest, run: &Run) -> Result<ToolDefinition> {
    let marker = format!("<harness kind=\"result_schema:{}\">", run.id);
    let body = request
        .messages
        .iter()
        .find_map(|message| {
            message
                .content
                .split_once(&marker)?
                .1
                .split_once("</harness>")
                .map(|(body, _)| body)
        })
        .expect("a changed or newly required schema is delivered at run start");
    let (encoded, definition) = body
        .lines()
        .find_map(|line| {
            serde_json::from_str::<ToolDefinition>(line)
                .ok()
                .map(|definition| (line, definition))
        })
        .expect("notice includes the full current ToolDefinition");
    assert_eq!(
        encoded,
        serde_json::to_string(&definition)?,
        "the full definition is compact JSON"
    );
    assert_eq!(definition.name, "submit_result");
    assert!(!definition.description.is_empty());
    assert_eq!(
        definition.input_schema,
        changed_options().output_schema.unwrap().schema
    );
    Ok(definition)
}

#[tokio::test]
async fn changed_and_new_result_contracts_use_run_notices_without_changing_chat_prefix()
-> Result<()> {
    for mode in [ExecutionMode::Native, ExecutionMode::Js] {
        for initially_required in [false, true] {
            for wrapped in [false, true] {
                let store = Store::in_memory()?;
                let session = store.create_session("changing result contract")?.id;
                let provider = Arc::new(Scripted {
                    replies: Mutex::new(
                        vec![
                            if initially_required {
                                result_call(mode, wrapped, json!({"answer":1}))
                            } else {
                                text("warm")
                            },
                            result_call(mode, wrapped, json!({"answer":2})),
                            result_call(mode, wrapped, json!({"answer":"current contract"})),
                            result_call(mode, wrapped, json!({"answer":3})),
                            text("ordinary reply"),
                            result_call(mode, wrapped, json!({"answer":"later result"})),
                        ]
                        .into(),
                    ),
                    ..Default::default()
                });
                let mut tools = ToolRegistry::new();
                tools.register(Arc::new(JsRoute))?;
                let harness = Harness::new(
                    store.clone(),
                    provider.clone(),
                    tools,
                    HarnessConfig {
                        execution_mode: mode,
                        system_prompt: "Stable instructions".into(),
                        ..Default::default()
                    },
                );
                let initial_options = if initially_required {
                    options()
                } else {
                    RunOptions::default()
                };
                let initial = harness.prompt(&session, "initial", initial_options).await?;
                assert_eq!(initial.status, "completed", "{:?}", initial.error);
                let changed = harness
                    .prompt(&session, "new contract", changed_options())
                    .await?;
                assert_eq!(changed.status, "completed", "{:?}", changed.error);
                assert_eq!(
                    changed.text.as_deref(),
                    Some(r#"{"answer":"current contract"}"#)
                );
                let plain = harness
                    .prompt(&session, "no result required", RunOptions::default())
                    .await?;
                assert_eq!(plain.status, "completed", "{:?}", plain.error);
                assert_eq!(plain.text.as_deref(), Some("ordinary reply"));
                let repeated = harness
                    .prompt(&session, "same changed contract", changed_options())
                    .await?;
                assert_eq!(repeated.status, "completed", "{:?}", repeated.error);
                let requests = provider.requests.lock().unwrap();
                assert_eq!(requests.len(), 6);
                for request in requests.iter() {
                    assert_eq!(prefix(request)?, prefix(&requests[0])?);
                    assert!(request.output_schema.is_none());
                }
                schema_notice(&requests[1], &changed)?;
                schema_notice(&requests[5], &repeated)?;
                assert_eq!(
                    serde_json::to_vec(
                        &harness.effective_tool_definitions(&session, &changed_options())?
                    )?,
                    serde_json::to_vec(&requests[0].tools)?
                );
                let history = store.messages(&session)?;
                assert!(history.iter().any(|message| {
                    message.role == Role::Tool
                        && message.is_error
                        && message
                            .content
                            .contains("Invalid arguments for submit_result")
                }));
                assert!(
                    history.iter().any(|message| message.role == Role::Tool
                        && message.is_error
                        && message
                            .content
                            .contains("submit_result requires a run output schema")),
                    "a declaration retained from the initial run grants no capability to an ordinary run"
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn changed_result_schema_after_store_reopen_keeps_the_initial_native_declaration()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("result.db");
    let session;
    let initial_prefix;
    {
        let store = Store::open(&path)?;
        session = store.create_session("result contract restart")?.id;
        let provider = Arc::new(Scripted {
            replies: Mutex::new(vec![submit(json!({"answer":1}))].into()),
            ..Default::default()
        });
        let harness = Harness::new(
            store,
            provider.clone(),
            ToolRegistry::new(),
            HarnessConfig::default(),
        );
        let run = harness.prompt(&session, "initial", options()).await?;
        assert_eq!(run.status, "completed", "{:?}", run.error);
        initial_prefix = prefix(&provider.requests.lock().unwrap()[0])?;
        harness.shutdown(std::time::Duration::from_secs(5)).await?;
    }
    let store = Store::open(&path)?;
    let provider = Arc::new(Scripted {
        replies: Mutex::new(
            vec![
                submit(json!({"answer":2})),
                result_call(
                    ExecutionMode::Native,
                    true,
                    json!({"answer":"restored current contract"}),
                ),
            ]
            .into(),
        ),
        ..Default::default()
    });
    let harness = Harness::new(
        store,
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    let run = harness
        .prompt(
            &session,
            "changed contract after restart",
            changed_options(),
        )
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(prefix(request)?, initial_prefix);
    }
    schema_notice(&requests[0], &run)?;
    Ok(())
}

#[derive(Default)]
struct FoldCoveredHistory {
    prepares: AtomicUsize,
    applied: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl ContextPreparation for FoldCoveredHistory {
    async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
        let mut request = call.request;
        if self.prepares.fetch_add(1, Ordering::SeqCst) >= 2 {
            let task = request
                .messages
                .iter()
                .rev()
                .find(|message| {
                    message.role == Role::User && message.origin != Some(MessageOrigin::Generated)
                })
                .unwrap()
                .clone();
            let mut memory = task.clone();
            memory.id = "context:applied-memory".into();
            memory.role = Role::System;
            memory.origin = Some(MessageOrigin::Generated);
            memory.content =
                "APPLIED_MEMORY: the fixture effect completed; finish the current task.".into();
            // The applied projection covers the old schema notice as well as
            // the completed work. Original rows remain in the authoritative store.
            request.messages = vec![memory, task];
        }
        Ok(PreparedContext {
            request,
            notice: None,
            usage: None,
        })
    }
    fn take_prefix_change(&self, _: &str) -> Option<String> {
        (self.prepares.load(Ordering::SeqCst) >= 3 && !self.applied.swap(true, Ordering::SeqCst))
            .then(|| "lcm_apply".into())
    }
    fn cold_message_indexes(&self, _: &str, request: &ModelRequest) -> Result<Vec<usize>> {
        Ok(request
            .messages
            .first()
            .is_some_and(|message| message.id == "context:applied-memory")
            .then_some(0)
            .into_iter()
            .collect())
    }
}

#[tokio::test]
async fn a_late_result_contract_is_refreshed_when_apply_folds_its_first_notice() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("schema notice apply")?.id;
    let provider = Arc::new(Scripted {
        replies: Mutex::new(
            vec![
                text("warm ordinary chat"),
                batch(vec![call("effect", json!({}))]),
                result_call(ExecutionMode::Native, true, json!({"answer":"after apply"})),
            ]
            .into(),
        ),
        ..Default::default()
    });
    let effect = Arc::new(Effect::default());
    let mut tools = ToolRegistry::new();
    tools.register_host(effect.clone())?;
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        tools,
        HarnessConfig {
            context: Arc::new(FoldCoveredHistory::default()),
            ..Default::default()
        },
    );
    assert_eq!(
        harness
            .prompt(&session, "warm", RunOptions::default())
            .await?
            .status,
        "completed"
    );
    let mut required = changed_options();
    required.tool_names = None;
    let run = harness
        .prompt(&session, "finish after doing work", required)
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(effect.0.load(Ordering::SeqCst), 1);
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let first_notice = schema_notice(&requests[1], &run)?;
    assert_eq!(
        serde_json::to_value(schema_notice(&requests[2], &run)?)?,
        serde_json::to_value(first_notice)?
    );
    let marker = format!("<harness kind=\"result_schema:{}\">", run.id);
    let covered = requests[1]
        .messages
        .iter()
        .find(|message| message.content.contains(&marker))
        .unwrap();
    assert!(
        !requests[2]
            .messages
            .iter()
            .any(|message| message.id == covered.id),
        "the old notice was folded; the current contract needs a fresh delivery"
    );
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|message| message.content.contains("APPLIED_MEMORY:"))
    );
    assert!(
        !requests[2]
            .messages
            .iter()
            .any(|message| !message.tool_calls.is_empty())
    );
    assert!(requests[2].cache_tiers.unwrap().cold_end.is_some());
    for request in requests.iter() {
        assert_eq!(prefix(request)?, prefix(&requests[0])?);
        assert!(
            !request
                .tools
                .iter()
                .any(|tool| tool.name == "submit_result")
        );
    }
    assert!(
        store
            .messages(&session)?
            .iter()
            .any(|message| message.id == covered.id)
    );
    Ok(())
}

#[derive(Default)]
struct ReservedCollision(AtomicUsize);
#[async_trait]
impl Tool for ReservedCollision {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "submit_result".into(),
            description: "Host collision must never execute".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Approval::None,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text("HOST_COLLISION_EXECUTED"))
    }
}

#[tokio::test]
async fn a_host_collision_cannot_make_a_stale_result_declaration_callable_without_a_schema()
-> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("reserved result collision")?.id;
    let provider = Arc::new(Scripted {
        replies: Mutex::new(
            vec![
                submit(json!({"answer":1})),
                submit(json!({"answer":2})),
                text("ordinary completion"),
            ]
            .into(),
        ),
        ..Default::default()
    });
    let collision = Arc::new(ReservedCollision::default());
    let mut tools = ToolRegistry::new();
    tools.register_host(collision.clone())?;
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        tools,
        HarnessConfig::default(),
    );
    let initial = harness
        .prompt(&session, "required result", options())
        .await?;
    assert_eq!(initial.status, "completed", "{:?}", initial.error);
    let plain = harness
        .prompt(&session, "ordinary task", RunOptions::default())
        .await?;
    assert_eq!(plain.status, "completed", "{:?}", plain.error);
    assert_eq!(plain.text.as_deref(), Some("ordinary completion"));
    assert_eq!(collision.0.load(Ordering::SeqCst), 0);
    let requests = provider.requests.lock().unwrap();
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|tool| tool.name == "submit_result")
    );
    for request in requests.iter() {
        assert_eq!(prefix(request)?, prefix(&requests[0])?);
    }
    assert!(store.messages(&session)?.iter().any(|message| {
        message.is_error
            && message
                .content
                .contains("submit_result requires a run output schema")
    }));
    Ok(())
}
