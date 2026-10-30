use super::*;
use anyhow::Result;
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

#[path = "tests_policy.rs"]
mod policy;

fn harness(store: Store, provider: Arc<dyn Provider>, tools: ToolRegistry) -> Harness {
    Harness::new(store, provider, tools, HarnessConfig::default())
}
fn answer(text: &str) -> ModelResponse {
    ModelResponse {
        text: text.into(),
        ..Default::default()
    }
}
fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "native".into(),
        arguments: serde_json::json!({"value":"host value"}),
    }
}
fn definition() -> ToolDefinition {
    ToolDefinition {
        approval: Default::default(),
        name: "native".into(),
        description: "Native host tool".into(),
        input_schema: serde_json::json!({"type":"object"}),
        read_only: false,
    }
}

#[test]
fn context_view_masks_and_clips_only_canonical_raw_tool_records() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("tools")?;
    let foreign = store.create_session("foreign tools")?;
    let assistant = Message {
        id: id(),
        session_id: session.id.clone(),
        role: Role::Assistant,
        origin: None,
        content: String::new(),
        tool_calls: vec![call("raw-call")],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: now(),
    };
    let output = Message {
        id: id(),
        role: Role::Tool,
        tool_calls: Vec::new(),
        tool_call_id: Some("raw-call".into()),
        content: "complete tool output".into(),
        ..assistant.clone()
    };
    store.append(&assistant)?;
    store.append(&output)?;
    let foreign_assistant = Message {
        id: id(),
        session_id: foreign.id.clone(),
        tool_calls: vec![call("foreign-call")],
        ..assistant.clone()
    };
    let foreign_output = Message {
        id: id(),
        session_id: foreign.id.clone(),
        tool_call_id: Some("foreign-call".into()),
        ..output.clone()
    };
    store.append(&foreign_assistant)?;
    store.append(&foreign_output)?;
    let saved = ContextView {
        session_id: session.id.clone(),
        frontier: None,
        system_prompt: "Frozen instructions".into(),
        prefix: Vec::new(),
        masked_tool_output_ids: vec![output.id.clone()],
        clipped_tool_call_ids: vec!["raw-call".into()],
        created_at: now(),
        threshold: 200_000,
        budget: 100_000,
        tokens: 20,
        blocks: Vec::new(),
        blocks_estimated: true,
    };
    let before = store.storage_stats()?.transaction_count;
    store.save_context_view(&saved)?;
    let baseline = serde_json::to_value(&saved)?;
    let mut initial_shape = baseline.clone();
    initial_shape
        .as_object_mut()
        .unwrap()
        .remove("system_prompt");
    assert!(
        serde_json::from_value::<ContextView>(initial_shape)?
            .system_prompt
            .is_empty()
    );
    assert_eq!(
        serde_json::to_value(store.context_view(&session.id)?.unwrap())?,
        baseline
    );
    for bad in [
        ContextView {
            masked_tool_output_ids: vec![foreign_output.id.clone()],
            ..saved.clone()
        },
        ContextView {
            clipped_tool_call_ids: vec!["foreign-call".into()],
            ..saved.clone()
        },
        ContextView {
            masked_tool_output_ids: vec![assistant.id.clone()],
            ..saved.clone()
        },
    ] {
        assert!(store.save_context_view(&bad).is_err());
        assert_eq!(store.storage_stats()?.transaction_count, before + 1);
        assert_eq!(
            serde_json::to_value(store.context_view(&session.id)?.unwrap())?,
            baseline
        );
    }
    assert!(store.message_seq(&foreign.id, &output.id).is_err());
    assert!(
        store
            .context_view_frontier_message_id(
                &foreign.id,
                Some(store.message_seq(&session.id, &output.id)?)
            )
            .is_err()
    );
    store.save_context_checkpoint(&ContextCheckpoint {
        files_touched: Vec::new(),
        id: id(),
        session_id: session.id.clone(),
        through_message_id: output.id.clone(),
        model: "fixture".into(),
        summary: "legacy fixture".into(),
        created_at: now(),
        usage: None,
    })?;
    let frontier = Some(store.message_seq(&session.id, &output.id)?);
    for bad in [
        ContextView {
            frontier,
            clipped_tool_call_ids: Vec::new(),
            ..saved.clone()
        },
        ContextView {
            frontier,
            masked_tool_output_ids: Vec::new(),
            ..saved.clone()
        },
    ] {
        assert!(store.save_context_view(&bad).is_err());
        assert_eq!(
            serde_json::to_value(store.context_view(&session.id)?.unwrap())?,
            baseline
        );
    }
    let legacy_view = ContextView {
        frontier,
        masked_tool_output_ids: Vec::new(),
        clipped_tool_call_ids: Vec::new(),
        ..saved
    };
    store.save_context_view(&legacy_view)?;
    assert_eq!(
        store.messages(&session.id)?[1].content,
        "complete tool output"
    );
    Ok(())
}

struct Guarded(Arc<AtomicUsize>);
#[async_trait]
impl Tool for Guarded {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            approval: Default::default(),
            name: "guarded".into(),
            description: "guarded read".into(),
            input_schema: serde_json::json!({"type":"object","properties":{"count":{"type":"integer","minimum":1}},"required":["count"],"additionalProperties":false}),
            read_only: true,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(ToolResult::text("nested read payload"))
    }
}
struct Composite;
#[async_trait]
impl Tool for Composite {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            approval: Default::default(),
            name: "composite".into(),
            description: "composition".into(),
            input_schema: serde_json::json!({"type":"object"}),
            read_only: true,
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
        assert!(
            dispatcher
                .invoke(
                    context.clone(),
                    "guarded",
                    serde_json::json!({"count":"invalid"})
                )
                .await?
                .is_error
        );
        let read = dispatcher
            .invoke(context.clone(), "guarded", serde_json::json!({"count":1}))
            .await?;
        assert_eq!(read.content, "nested read payload");
        dispatcher.emit_output(
            &context,
            "cell-1",
            serde_json::json!({"type":"text","text":"published"}),
        )?;
        Ok(ToolResult::text("composition finished"))
    }
}
struct HostBridge;
#[async_trait]
impl Tool for HostBridge {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            approval: Default::default(),
            name: "compose".into(),
            description: "Test host composition entrypoint".into(),
            input_schema: serde_json::json!({"type":"object","properties":{"target":{"type":"string"},"input":{"type":"object"}},"required":["target","input"]}),
            read_only: true,
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
        assert!(
            dispatcher
                .definitions()
                .iter()
                .all(|tool| tool.name != "compose")
        );
        dispatcher
            .invoke(
                context,
                input["target"].as_str().unwrap(),
                input["input"].clone(),
            )
            .await
    }
}
fn bridge_call(id: &str, target: &str, input: Value) -> ModelResponse {
    ModelResponse {
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "compose".into(),
            arguments: serde_json::json!({"target":target,"input":input}),
        }],
        ..Default::default()
    }
}
#[derive(Default)]
struct RecordedObservations(Mutex<Vec<ObservationRecord>>);
impl ObservationSink for RecordedObservations {
    fn record(&self, record: ObservationRecord) {
        self.0.lock().unwrap().push(record);
    }
}

#[test]
fn registry_registration_keeps_model_and_host_access_distinct() -> Result<()> {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(HostBridge))?;
    tools.register_host(Arc::new(Guarded(Arc::new(AtomicUsize::new(0)))))?;
    assert!(tools.register_host(Arc::new(HostBridge)).is_err());
    assert!(
        tools
            .register(Arc::new(Guarded(Arc::new(AtomicUsize::new(0)))))
            .is_err()
    );
    assert_eq!(tools.definitions()[0].name, "compose");
    assert_eq!(tools.capabilities()[0].name, "guarded");
    assert!(tools.get_model("guarded").is_none());
    assert!(tools.get_host("compose").is_none());
    let only_host_name = tools.restricted(&["guarded".into()]);
    assert!(only_host_name.definitions().is_empty());
    assert_eq!(only_host_name.capabilities().len(), 1);
    let no_hosts = tools.restricted_capabilities(&[]);
    assert_eq!(no_hosts.definitions().len(), 1);
    assert!(no_hosts.capabilities().is_empty());
    Ok(())
}

#[tokio::test]
async fn direct_schema_failure_never_invokes_tool() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("invalid arguments")?;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Guarded(calls.clone())))?;
    let provider = Arc::new(Script::new(vec![
        ModelResponse {
            tool_calls: vec![ToolCall {
                id: "bad-input".into(),
                name: "guarded".into(),
                arguments: serde_json::json!({"count":0}),
            }],
            ..Default::default()
        },
        answer("recovered"),
    ]));
    let agent = harness(store.clone(), provider, tools);
    assert_eq!(
        agent
            .prompt(&session.id, "start", RunOptions::default())
            .await?
            .status,
        "completed"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    let rejected = &store.messages(&session.id)?[2];
    assert!(rejected.is_error);
    assert!(rejected.content.contains("guarded at /count"));
    Ok(())
}

#[test]
fn unknown_arguments_name_valid_fields_without_echoing_values() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register_host(Arc::new(Guarded(calls.clone())))?;
    for input in [
        serde_json::json!({"count":1,"typo":"private-value"}),
        serde_json::json!({"typo":"private-value"}),
    ] {
        let error = tools.validate("guarded", &input).unwrap_err().to_string();
        assert!(error.contains("unexpected arguments: typo"), "{error}");
        assert!(error.contains("Valid arguments: count."), "{error}");
        assert!(!error.contains("private-value"), "{error}");
    }
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    Ok(())
}

#[test]
fn unknown_nested_arguments_use_the_closed_objects_own_fields() -> Result<()> {
    struct Nested;
    #[async_trait]
    impl Tool for Nested {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "nested".into(),
                description: "Nested schema fixture".into(),
                approval: Default::default(),
                read_only: true,
                input_schema: serde_json::json!({
                    "type":"object",
                    "properties": {
                        "items": {
                            "type":"array",
                            "items": {
                                "type":"object",
                                "properties":{"offset":{"type":"integer"}},
                                "additionalProperties":false
                            }
                        },
                        "empty":{"type":"object","additionalProperties":false}
                    },
                    "additionalProperties":false
                }),
            }
        }
        async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
            unreachable!("invalid arguments are rejected before invocation")
        }
    }
    let mut tools = ToolRegistry::new();
    tools.register_host(Arc::new(Nested))?;
    let error = tools
        .validate(
            "nested",
            &serde_json::json!({"items":[{"typo":"private-value"}]}),
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("nested at /items/0"), "{error}");
    assert!(error.contains("Valid arguments: offset."), "{error}");
    assert!(!error.contains("private-value"), "{error}");
    let error = tools
        .validate("nested", &serde_json::json!({"empty":{"typo":1}}))
        .unwrap_err()
        .to_string();
    assert!(error.contains("Valid arguments: (none)."), "{error}");
    Ok(())
}

#[tokio::test]
async fn union_argument_error_names_field_and_types_then_allows_correction() -> Result<()> {
    struct CommandInput(Arc<AtomicUsize>);
    #[async_trait]
    impl Tool for CommandInput {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                approval: Default::default(),
                name: "command_input".into(),
                description: "Schema regression fixture".into(),
                read_only: true,
                input_schema: serde_json::json!({"type":"object","properties":{"command":{"oneOf":[{"type":"string"},{"type":"object"}]}},"required":["command"],"additionalProperties":false}),
            }
        }
        async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(ToolResult::text("accepted"))
        }
    }
    let store = Store::in_memory()?;
    let session = store.create_session("correctable schema error")?;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(CommandInput(calls.clone())))?;
    let call = |id: &str, command: Value| ModelResponse {
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "command_input".into(),
            arguments: serde_json::json!({"command":command}),
        }],
        ..Default::default()
    };
    let provider = Arc::new(Script::new(vec![
        call("bad", serde_json::json!(123)),
        call("corrected", serde_json::json!("echo ok")),
        answer("recovered"),
    ]));
    let agent = harness(store.clone(), provider.clone(), tools);
    assert_eq!(
        agent
            .prompt(&session.id, "start", RunOptions::default())
            .await?
            .status,
        "completed"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let messages = store.messages(&session.id)?;
    assert!(messages[2].is_error);
    assert!(messages[2].content.contains("command_input at /command"));
    assert!(messages[2].content.contains("string or object"));
    assert_eq!(messages[4].content, "accepted");
    assert!(!messages[4].is_error);
    assert!(store.pending_effects(&session.id)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn bounded_snapshots_preserve_counts_and_never_write() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("bounded history")?;
    let provider = Arc::new(Script::new(vec![
        answer("one"),
        answer("two"),
        answer("three"),
    ]));
    let harness = harness(store.clone(), provider, ToolRegistry::new());
    for text in ["first", "second", "third"] {
        harness
            .prompt(&session.id, text, RunOptions::default())
            .await?;
    }
    store.queue(&session.id, InputKind::Followup, "later")?;
    store.queue(&session.id, InputKind::Steer, "direction")?;
    let revision = store.revision();
    let snapshot = store.snapshot(&session.id, 2, 1, 1)?;
    assert_eq!(snapshot.revision, revision);
    assert_eq!(snapshot.message_count, 6);
    assert_eq!(snapshot.run_count, 3);
    assert_eq!(snapshot.pending_input_count, 2);
    assert_eq!(
        snapshot
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["third", "three"]
    );
    assert_eq!(snapshot.runs.len(), 1);
    assert_eq!(snapshot.runs[0].status, "completed");
    assert_eq!(snapshot.pending_inputs[0].text, "later");
    let empty = store.snapshot(&session.id, 0, 0, 0)?;
    assert!(empty.messages.is_empty() && empty.runs.is_empty() && empty.pending_inputs.is_empty());
    assert_eq!(empty.message_count, 6);
    let summaries = store.session_summaries(1)?;
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].last_message.as_ref().unwrap().content, "three");
    assert_eq!(
        summaries[0].latest_run.as_ref().unwrap().status,
        "completed"
    );
    assert_eq!(summaries[0].pending_input_count, 2);
    assert!(store.session_summaries(0)?.is_empty());
    assert!(store.snapshot("missing", 2, 1, 1).is_err());
    assert_eq!(store.revision(), revision);
    store.create_session("newer but idle")?;
    store.append(&Store::message(
        &session.id,
        Role::Assistant,
        "recent activity".into(),
    ))?;
    assert_eq!(store.session_summaries(1)?[0].session.id, session.id);
    Ok(())
}
struct Script {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}

#[tokio::test]
async fn default_round_limit_allows_more_than_thirty_two_requests() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("long root run")?;
    let mut responses = (0..40)
        .map(|round| ModelResponse {
            tool_calls: vec![call(&format!("round-{round}"))],
            ..Default::default()
        })
        .collect::<Vec<_>>();
    responses.push(answer("finished naturally"));
    let provider = Arc::new(Script::new(responses));
    let agent = harness(store.clone(), provider.clone(), ToolRegistry::new());
    assert_eq!(agent.max_rounds(), None);
    let run = agent
        .prompt(&session.id, "start", RunOptions::default())
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(run.text.as_deref(), Some("finished naturally"));
    assert_eq!(provider.requests.lock().unwrap().len(), 41);
    assert_eq!(
        store
            .messages(&session.id)?
            .iter()
            .filter(|m| m.role == Role::Tool)
            .count(),
        40
    );
    Ok(())
}

#[tokio::test]
async fn per_run_round_limit_overrides_default_and_rejects_zero_before_admission() -> Result<()> {
    for (limit, expected) in [(Some(1), "failed"), (Some(2), "completed")] {
        let store = Store::in_memory()?;
        let session = store.create_session("round limit")?;
        let provider = Arc::new(Script::new(vec![
            ModelResponse {
                tool_calls: vec![call("unregistered")],
                ..Default::default()
            },
            answer("corrected"),
        ]));
        let agent = Harness::new(
            store.clone(),
            provider.clone(),
            ToolRegistry::new(),
            HarnessConfig {
                max_rounds: Some(1),
                ..Default::default()
            },
        );
        assert_eq!(agent.max_rounds(), Some(1));
        assert!(
            agent
                .start(
                    &session.id,
                    "invalid",
                    RunOptions {
                        max_rounds: Some(0),
                        ..Default::default()
                    }
                )
                .is_err()
        );
        assert!(store.messages(&session.id)?.is_empty());
        assert!(store.runs(&session.id)?.is_empty());
        let run = agent
            .prompt(
                &session.id,
                "start",
                RunOptions {
                    max_rounds: limit,
                    ..Default::default()
                },
            )
            .await?;
        assert_eq!(run.status, expected);
        assert_eq!(provider.requests.lock().unwrap().len(), limit.unwrap());
    }
    Ok(())
}
impl Script {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::default(),
        }
    }
}
#[async_trait]
impl Provider for Script {
    async fn generate(
        &self,
        request: ModelRequest,
        callback: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("script exhausted");
        callback(&response.text);
        Ok(response)
    }
}
struct Native {
    contexts: Mutex<Vec<ToolContext>>,
    result: String,
}
#[async_trait]
impl Tool for Native {
    fn definition(&self) -> ToolDefinition {
        definition()
    }
    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        assert_eq!(input["value"], "host value");
        assert!(!context.invocation_id.is_empty());
        self.contexts.lock().unwrap().push(context);
        Ok(ToolResult::text(&self.result))
    }
}

#[tokio::test]
async fn native_tool_loop_persists_provider_state_and_bound_result_after_reopen() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("archive.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("tool loop")?;
    let provider = Arc::new(Script::new(vec![
        ModelResponse {
            text: "Using the tool.".into(),
            tool_calls: vec![call("call-1")],
            provider_state: Some(serde_json::json!({"opaque":[{"type":"reasoning","id":"r"}]})),
            ..Default::default()
        },
        answer("Finished."),
    ]));
    let native = Arc::new(Native {
        contexts: Mutex::default(),
        result: "猫".repeat(1000),
    });
    let mut tools = ToolRegistry::new();
    tools.register(native.clone())?;
    assert!(tools.register(native.clone()).is_err());
    let agent = Harness::new(
        store.clone(),
        provider.clone(),
        tools,
        HarnessConfig {
            max_tool_result_bytes: 101,
            ..Default::default()
        },
    );
    let run = agent
        .prompt(&session.id, "Start", RunOptions::default())
        .await?;
    assert_eq!(run.status, "completed");
    assert_eq!(run.text.as_deref(), Some("Finished."));
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages[1].provider_state.is_some());
    assert!(requests[1].messages[2].content.len() <= 101);
    drop(requests);
    assert_eq!(native.contexts.lock().unwrap()[0].session_id, session.id);
    drop(agent);
    drop(store);
    let reopened = Store::open(&path)?;
    let messages = reopened.messages(&session.id)?;
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("call-1"));
    assert!(reopened.run(&run.id)?.text.is_none());
    assert!(
        reopened
            .queued_inputs(&session.id)?
            .iter()
            .all(|i| i.text.is_empty())
    );
    Ok(())
}

struct Deltas {
    store: Store,
}
#[async_trait]
impl Provider for Deltas {
    async fn generate(
        &self,
        _: ModelRequest,
        on_text: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let before = self.store.storage_stats()?.transaction_count;
        for _ in 0..20_000 {
            on_text("x");
        }
        assert_eq!(self.store.storage_stats()?.transaction_count, before);
        Ok(answer(&"x".repeat(20_000)))
    }
}
#[tokio::test]
async fn stream_tokens_are_transient_and_events_are_bounded() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("stream")?;
    let agent = Harness::new(
        store.clone(),
        Arc::new(Deltas {
            store: store.clone(),
        }),
        ToolRegistry::new(),
        HarnessConfig {
            max_paragraph_bytes: 128,
            event_capacity: 65536,
            ..Default::default()
        },
    );
    let mut events = agent.subscribe();
    let before = store.storage_stats()?.transaction_count;
    let run = agent
        .prompt(&session.id, "hello", RunOptions::default())
        .await?;
    assert_eq!(run.status, "completed");
    assert_eq!(store.storage_stats()?.transaction_count - before, 2);
    assert_eq!(store.messages(&session.id)?.len(), 2);
    let mut paragraphs = 0;
    let mut progress_bytes = 0;
    while let Ok(event) = events.try_recv() {
        match event.kind {
            EventKind::Paragraph { text } => {
                assert!(text.len() <= 128);
                paragraphs += 1;
            }
            EventKind::TextDelta { text } => {
                assert!(!event.durable);
                progress_bytes += text.len();
            }
            _ => {}
        }
    }
    assert!(paragraphs > 1);
    assert!(paragraphs < 200);
    // Every provider batch reaches the host as progress, and none of it is stored.
    assert_eq!(progress_bytes, 20_000);
    Ok(())
}

struct PendingProvider {
    entered: Arc<Notify>,
}
#[async_trait]
impl Provider for PendingProvider {
    async fn generate(
        &self,
        _: ModelRequest,
        on_text: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        on_text("discard unfinished stream");
        self.entered.notify_one();
        std::future::pending().await
    }
}
#[tokio::test]
async fn pending_provider_cancellation_is_bounded_and_fragments_are_not_stored() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("stop")?;
    let entered = Arc::new(Notify::new());
    let agent = harness(
        store.clone(),
        Arc::new(PendingProvider {
            entered: entered.clone(),
        }),
        ToolRegistry::new(),
    );
    let run = agent.start(&session.id, "hello", RunOptions::default())?;
    entered.notified().await;
    assert!(agent.cancel(&session.id));
    let finished =
        tokio::time::timeout(Duration::from_secs(1), agent.wait_for_idle(&session.id)).await??;
    assert_eq!(finished.id, run.id);
    assert_eq!(finished.status, "cancelled");
    assert_eq!(store.messages(&session.id)?.len(), 1);
    agent.shutdown(Duration::from_secs(1)).await?;
    Ok(())
}
struct CancelHookProvider {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}
#[async_trait]
impl Provider for CancelHookProvider {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(answer("released by host cancellation"))
    }
}
struct CancelAndAwaitHook {
    release: Arc<Notify>,
    finished: Mutex<std::sync::mpsc::Receiver<HarnessEvent>>,
    observed: Arc<Mutex<Option<HarnessEvent>>>,
}
#[async_trait]
impl Tool for CancelAndAwaitHook {
    fn definition(&self) -> ToolDefinition {
        definition()
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        anyhow::bail!("fixture hook is never invoked as a tool")
    }
    fn cancel_session(&self, _: &str) -> usize {
        self.release.notify_one();
        // Force the provider's terminal checkpoint to happen inside the hook.
        // With hook-first cancellation this deterministically stores completed.
        *self.observed.lock().unwrap() = self
            .finished
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .ok();
        1
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_signals_run_before_host_hook_unblocks_completion() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("cancel ordering")?;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let observed = Arc::new(Mutex::new(None));
    let (send, receive) = std::sync::mpsc::channel();
    let mut tools = ToolRegistry::new();
    tools.register_host(Arc::new(CancelAndAwaitHook {
        release: release.clone(),
        finished: Mutex::new(receive),
        observed: observed.clone(),
    }))?;
    let agent = Arc::new(harness(
        store.clone(),
        Arc::new(CancelHookProvider {
            entered: entered.clone(),
            release,
        }),
        tools,
    ));
    let mut events = agent.subscribe();
    let terminal = tokio::spawn(async move {
        loop {
            let event = events.recv().await?;
            if matches!(&event.kind, EventKind::RunFinished { .. }) {
                send.send(event)?;
                return Ok::<_, anyhow::Error>(());
            }
        }
    });
    let run = agent.start(
        &session.id,
        "hold until host cancellation",
        RunOptions::default(),
    )?;
    tokio::time::timeout(Duration::from_secs(2), entered.notified()).await?;
    let cancel_agent = agent.clone();
    let session_id = session.id.clone();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(3),
            tokio::task::spawn_blocking(move || {
                cancel_agent.cancel_from(
                    &session_id,
                    Some(ClientInfo {
                        id: "cancel-client".into(),
                        name: None,
                    }),
                )
            })
        )
        .await??
    );
    terminal.await??;
    let finished = agent.wait_for_idle(&session.id).await?;
    assert_eq!(finished.id, run.id);
    assert_eq!(finished.status, "cancelled");
    assert_eq!(store.run(&run.id)?.status, "cancelled");
    let event = observed
        .lock()
        .unwrap()
        .take()
        .expect("host hook observed terminal commit before returning");
    assert!(
        matches!(event.kind, EventKind::RunFinished { ref status, .. } if status == "cancelled")
    );
    assert_eq!(
        event.origin.as_ref().map(|origin| origin.id.as_str()),
        Some("cancel-client")
    );
    assert_eq!(
        store.messages(&session.id)?.len(),
        1,
        "cancelled provider reply is not accepted"
    );
    Ok(())
}

#[path = "tests_cancellation.rs"]
mod cancellation;

#[tokio::test]
async fn actual_process_style_restart_closes_unknown_and_undispatched_effects_without_replay()
-> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("archive.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("restart")?;
    let run = Run {
        id: id(),
        session_id: session.id.clone(),
        model: "test".into(),
        status: "running".into(),
        text: None,
        error: None,
        started_at: now(),
        ended_at: None,
        usage: None,
    };
    store.start_run(&run)?;
    let mut assistant = Store::message(&session.id, Role::Assistant, String::new());
    assistant.tool_calls = vec![call("dispatched"), call("not-dispatched")];
    assistant.provider_state = Some(serde_json::json!({"opaque":"retained"}));
    store.append(&assistant)?;
    store.dispatch(&session.id, &run.id, &assistant.tool_calls[0])?;
    store.queue(&session.id, InputKind::Followup, "recover next")?;
    drop(store);
    let reopened = Store::open(&path)?;
    assert_eq!(reopened.run(&run.id)?.status, "interrupted");
    let messages = reopened.messages(&session.id)?;
    assert_eq!(messages.len(), 3);
    assert!(messages[1].content.contains("outcome unknown"));
    assert!(messages[2].content.contains("not dispatched"));
    let native = Arc::new(Native {
        contexts: Mutex::default(),
        result: "unexpected".into(),
    });
    let mut tools = ToolRegistry::new();
    tools.register(native.clone())?;
    let provider = Arc::new(Script::new(vec![answer("acknowledged")]));
    let agent = harness(reopened.clone(), provider.clone(), tools);
    assert_eq!(
        agent
            .resume(&session.id, RunOptions::default())
            .await?
            .status,
        "completed"
    );
    assert!(native.contexts.lock().unwrap().is_empty());
    assert_eq!(
        provider.requests.lock().unwrap()[0].messages[0].provider_state,
        assistant.provider_state
    );
    Ok(())
}

#[tokio::test]
async fn provider_cannot_reissue_an_unknown_invocation_id() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("unknown call")?;
    let run = Run {
        id: id(),
        session_id: session.id.clone(),
        model: "test".into(),
        status: "running".into(),
        text: None,
        error: None,
        started_at: now(),
        ended_at: None,
        usage: None,
    };
    store.start_run(&run)?;
    let mut assistant = Store::message(&session.id, Role::Assistant, String::new());
    assistant.tool_calls = vec![call("unknown-call")];
    store.append(&assistant)?;
    store.dispatch(&session.id, &run.id, &assistant.tool_calls[0])?;
    store.close_unresolved(&session.id, &run.id)?;
    let mut interrupted = run;
    interrupted.status = "interrupted".into();
    interrupted.ended_at = Some(now());
    store.finish_run(&interrupted)?;
    let native = Arc::new(Native {
        contexts: Mutex::default(),
        result: "unexpected".into(),
    });
    let mut tools = ToolRegistry::new();
    tools.register(native.clone())?;
    let provider = Arc::new(Script::new(vec![ModelResponse {
        tool_calls: vec![call("unknown-call")],
        ..Default::default()
    }]));
    let agent = harness(store.clone(), provider, tools);
    let result = agent.resume(&session.id, RunOptions::default()).await?;
    assert_eq!(result.status, "failed");
    assert!(result.error.unwrap().contains("reused"));
    assert!(native.contexts.lock().unwrap().is_empty());
    assert_eq!(store.messages(&session.id)?.len(), 2);
    Ok(())
}

struct BoundaryProvider {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    requests: Mutex<Vec<ModelRequest>>,
    round: AtomicUsize,
}
#[async_trait]
impl Provider for BoundaryProvider {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        let round = self.round.fetch_add(1, Ordering::SeqCst);
        if round == 0 {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(answer(&format!("answer {round}")))
    }
}
#[tokio::test]
async fn steering_and_followups_are_durable_and_obey_round_boundaries() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("archive.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("queue")?;
    let provider = Arc::new(BoundaryProvider {
        entered: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
        requests: Mutex::default(),
        round: AtomicUsize::new(0),
    });
    let agent = harness(store.clone(), provider.clone(), ToolRegistry::new());
    let mut events = agent.subscribe();
    let first = agent.start(&session.id, "initial", RunOptions::default())?;
    provider.entered.notified().await;
    agent.followup(&session.id, "followup")?;
    agent.steer(&session.id, "steer")?;
    assert_eq!(store.storage_stats()?.pending_input_count, 2);
    let second_handle = Store::open(&path)?;
    assert_eq!(second_handle.run(&first.id)?.status, "running");
    let other = harness(second_handle.clone(), provider.clone(), ToolRegistry::new());
    assert!(
        other
            .start_resume(&session.id, RunOptions::default())
            .is_err()
    );
    drop(other);
    drop(second_handle);
    provider.release.notify_one();
    let final_run = agent.wait_for_idle(&session.id).await?;
    assert_ne!(first.id, final_run.id);
    assert_eq!(final_run.status, "completed");
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].messages.last().unwrap().content, "initial");
    assert_eq!(requests[1].messages.last().unwrap().content, "steer");
    assert_eq!(requests[2].messages.last().unwrap().content, "followup");
    drop(requests);
    let mut finished = 0;
    let mut settled = 0;
    while let Ok(event) = events.try_recv() {
        match event.kind {
            EventKind::RunFinished { .. } => finished += 1,
            EventKind::SessionSettled {} => settled += 1,
            _ => {}
        }
    }
    assert_eq!(finished, 2);
    assert_eq!(settled, 1);
    assert_eq!(store.storage_stats()?.pending_input_count, 0);
    assert_eq!(store.runs(&session.id)?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn queued_input_survives_close_and_reopen_before_execution() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("archive.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("durable")?;
    store.queue(&session.id, InputKind::Followup, "first")?;
    store.queue(&session.id, InputKind::Followup, "second")?;
    drop(store);
    let store = Store::open(path)?;
    let provider = Arc::new(Script::new(vec![answer("a"), answer("b")]));
    let agent = harness(store.clone(), provider.clone(), ToolRegistry::new());
    assert_eq!(
        agent
            .resume(&session.id, RunOptions::default())
            .await?
            .text
            .as_deref(),
        Some("b")
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests[0].messages[0].content, "first");
    assert_eq!(requests[1].messages.last().unwrap().content, "second");
    Ok(())
}

#[derive(Default)]
struct PausedPreparation {
    entered: Notify,
    release: Notify,
    requests: Mutex<Vec<ModelRequest>>,
    fit_budget: Option<usize>,
    pending_estimate: AtomicUsize,
    observed_estimates: Mutex<Vec<usize>>,
    compressions: AtomicUsize,
    aux_usage_per_preparation: Option<Usage>,
}

struct SteeringWaitTool {
    owner: String,
    entered: Notify,
    release: Notify,
    interrupts: AtomicUsize,
}
#[async_trait]
impl Tool for SteeringWaitTool {
    fn definition(&self) -> ToolDefinition {
        definition()
    }
    async fn invoke(&self, context: ToolContext, _: Value) -> Result<ToolResult> {
        assert_eq!(context.session_id, self.owner);
        self.entered.notify_one();
        self.release.notified().await;
        assert!(!context.cancellation.is_cancelled());
        Ok(ToolResult::text("finished naturally"))
    }
    fn interrupt_wait(&self, session: &str) -> usize {
        if session != self.owner {
            return 0;
        }
        self.interrupts.fetch_add(1, Ordering::SeqCst);
        self.release.notify_one();
        1
    }
}

#[tokio::test]
async fn ordinary_steers_do_not_release_foreground_waits_or_add_a_generation() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("waiting")?;
    let tool = Arc::new(SteeringWaitTool {
        owner: session.id.clone(),
        entered: Notify::new(),
        release: Notify::new(),
        interrupts: AtomicUsize::new(0),
    });
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone())?;
    let provider = Arc::new(Script::new(vec![
        ModelResponse {
            tool_calls: vec![call("work")],
            ..Default::default()
        },
        answer("done"),
    ]));
    let agent = harness(store.clone(), provider.clone(), tools.clone());
    agent.start(&session.id, "initial", RunOptions::default())?;
    tool.entered.notified().await;
    agent.steer(&session.id, "first")?;
    agent.steer(&session.id, "second")?;
    tokio::task::yield_now().await;
    assert_eq!(tool.interrupts.load(Ordering::SeqCst), 0);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert_eq!(
        store
            .queued_inputs(&session.id)?
            .iter()
            .filter(|q| q.state == "pending")
            .count(),
        2
    );
    tool.release.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), agent.wait_for_idle(&session.id))
            .await??
            .status,
        "completed"
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let messages = &requests[1].messages;
    let tool_position = messages.iter().position(|m| m.role == Role::Tool).unwrap();
    assert_eq!(
        messages[tool_position + 1..]
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "second"]
    );
    drop(requests);
    assert_eq!(tools.interrupt_waits("unrelated"), 0);
    assert_eq!(
        tools.interrupt_waits(&session.id),
        1,
        "explicit host wait release remains available"
    );
    assert_eq!(tool.interrupts.load(Ordering::SeqCst), 1);
    Ok(())
}
#[async_trait]
impl ContextPreparation for PausedPreparation {
    async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
        let mut request = call.request;
        let first = self.requests.lock().unwrap().is_empty();
        self.requests.lock().unwrap().push(request.clone());
        if first {
            self.entered.notify_one();
            self.release.notified().await;
        }
        let size = |request: &ModelRequest| {
            request
                .messages
                .iter()
                .map(|m| m.content.len())
                .sum::<usize>()
        };
        if let Some(budget) = self.fit_budget {
            let total = size(&request);
            if total > budget {
                // Model a context policy that compresses an older prefix while
                // retaining both newly admitted user instructions unchanged.
                let excess = total - budget;
                anyhow::ensure!(request.messages[0].content.len() >= excess);
                let retained = request.messages[0].content.len() - excess;
                request.messages[0].content.truncate(retained);
                self.compressions.fetch_add(1, Ordering::SeqCst);
            }
        }
        self.pending_estimate
            .store(size(&request), Ordering::SeqCst);
        Ok(PreparedContext {
            request,
            usage: self.aux_usage_per_preparation.clone(),
            notice: None,
        })
    }
    fn observe_response(&self, _: &str, _: &ModelResponse) {
        self.observed_estimates
            .lock()
            .unwrap()
            .push(self.pending_estimate.load(Ordering::SeqCst));
    }
}

#[tokio::test]
async fn steers_accepted_during_preparation_join_imminent_main_request_once_in_order() -> Result<()>
{
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("steering.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("main")?;
    let other = store.create_session("other")?;
    let context = Arc::new(PausedPreparation {
        entered: Notify::new(),
        release: Notify::new(),
        requests: Mutex::default(),
        ..Default::default()
    });
    let provider = Arc::new(Script::new(vec![
        answer("main done"),
        answer("followup done"),
        answer("unexpected extra generation"),
    ]));
    let agent = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            context: context.clone(),
            ..Default::default()
        },
    );
    let mut events = agent.subscribe();
    agent.start(&session.id, "initial", RunOptions::default())?;
    context.entered.notified().await;
    let first = agent.steer_from(
        &session.id,
        "first steer",
        Some(ClientInfo {
            id: "phone".into(),
            name: None,
        }),
    )?;
    let second = agent.steer_from(
        &session.id,
        "second steer",
        Some(ClientInfo {
            id: "desktop".into(),
            name: None,
        }),
    )?;
    agent.followup(&session.id, "followup")?;
    agent.steer(&other.id, "foreign steer")?;
    // Admission is durable while preparation is still waiting, with no call or cancellation.
    let reopened = Store::open(&path)?;
    let queued = reopened
        .queued_inputs(&session.id)?
        .into_iter()
        .filter(|q| q.state == "pending")
        .collect::<Vec<_>>();
    assert_eq!(queued.len(), 3);
    assert_eq!(queued[0].id, first.id);
    assert_eq!(queued[1].id, second.id);
    assert!(provider.requests.lock().unwrap().is_empty());
    assert_eq!(
        context.requests.lock().unwrap()[0].messages.len(),
        1,
        "in-flight preparation must keep its original input snapshot"
    );
    drop(reopened);
    context.release.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), agent.wait_for_idle(&session.id))
            .await??
            .status,
        "completed"
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "the batched steers must not create a separate generation"
    );
    assert_eq!(
        requests[0]
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>(),
        vec!["initial", "first steer", "second steer"]
    );
    assert!(
        requests[0].messages[1..]
            .iter()
            .all(|m| m.role == Role::User && m.origin == Some(MessageOrigin::Typed))
    );
    assert_eq!(
        requests[1]
            .messages
            .iter()
            .filter(|m| m.content == "first steer")
            .count(),
        1
    );
    assert_eq!(
        requests[1]
            .messages
            .iter()
            .filter(|m| m.content == "second steer")
            .count(),
        1
    );
    assert_eq!(requests[1].messages.last().unwrap().content, "followup");
    assert_eq!(store.queued_inputs(&other.id)?[0].state, "pending");
    let mut accepted_by = Vec::new();
    while let Ok(event) = events.try_recv() {
        if event.session_id == session.id
            && matches!(event.kind, EventKind::SteeringAccepted { .. })
        {
            accepted_by.push(event.origin.unwrap().id);
        }
    }
    assert_eq!(accepted_by, vec!["phone", "desktop"]);
    Ok(())
}

#[tokio::test]
async fn late_steering_is_reprepared_from_canonical_history_for_fit_and_usage_calibration()
-> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("late input admission")?;
    let context = Arc::new(PausedPreparation {
        fit_budget: Some(120),
        aux_usage_per_preparation: Some(Usage {
            input_tokens: 7,
            output_tokens: 1,
            ..Default::default()
        }),
        ..Default::default()
    });
    let provider = Arc::new(Script::new(vec![
        answer("done"),
        answer("unexpected extra generation"),
    ]));
    let agent = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            context: context.clone(),
            ..Default::default()
        },
    );
    let initial = "x".repeat(100);
    let first = "a".repeat(30);
    let second = "b".repeat(30);
    agent.start(&session.id, initial.clone(), RunOptions::default())?;
    context.entered.notified().await;
    agent.steer(&session.id, first.clone())?;
    agent.steer(&session.id, second.clone())?;
    context.release.notify_one();
    let run =
        tokio::time::timeout(Duration::from_secs(5), agent.wait_for_idle(&session.id)).await??;
    assert_eq!(run.status, "completed");
    assert_eq!(run.usage.as_ref().unwrap().input_tokens, 14);
    assert_eq!(run.usage.as_ref().unwrap().output_tokens, 2);
    let prepared = context.requests.lock().unwrap();
    assert_eq!(
        prepared.len(),
        2,
        "one fresh canonical re-preparation admits the late batch"
    );
    assert_eq!(
        prepared[1]
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>(),
        vec![initial.as_str(), first.as_str(), second.as_str()]
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]
            .messages
            .iter()
            .map(|m| m.content.len())
            .sum::<usize>(),
        120
    );
    assert_eq!(requests[0].messages[1].content, first);
    assert_eq!(requests[0].messages[2].content, second);
    assert_eq!(context.compressions.load(Ordering::SeqCst), 1);
    assert_eq!(*context.observed_estimates.lock().unwrap(), vec![120]);
    assert_eq!(
        store.messages(&session.id)?[0].content,
        initial,
        "context projection must not rewrite canonical history"
    );
    Ok(())
}

struct SnapshotProjectionProvider {
    projected: Option<usize>,
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Provider for SnapshotProjectionProvider {
    fn estimate_input_tokens(
        &self,
        _: &ModelRequest,
        _: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        Ok(self.projected)
    }
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        Ok(answer("done"))
    }
}
#[derive(Default)]
struct CapturedProjectionContext {
    entered: Notify,
    release: Notify,
    estimates: Mutex<Vec<usize>>,
}
impl CapturedProjectionContext {
    async fn bound(
        &self,
        request: ModelRequest,
        provider: Arc<dyn Provider>,
    ) -> Result<PreparedContext> {
        self.entered.notify_one();
        self.release.notified().await;
        let projected = provider
            .estimate_input_tokens(&request, &|_| 1)?
            .unwrap_or(1);
        self.estimates.lock().unwrap().push(projected);
        anyhow::ensure!(projected <= 100, "captured provider exceeds fit budget");
        Ok(PreparedContext {
            request,
            notice: Some("bound snapshot".into()),
            usage: None,
        })
    }
}
#[async_trait]
impl ContextPreparation for CapturedProjectionContext {
    async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
        self.bound(call.request, call.provider).await
    }
}

#[tokio::test]
async fn captured_provider_projection_survives_reload_during_async_preparation() -> Result<()> {
    for projected in [80, 200] {
        let store = Store::in_memory()?;
        let session = store.create_session("bound provider")?;
        let old = Arc::new(SnapshotProjectionProvider {
            projected: Some(projected),
            requests: Mutex::default(),
        });
        let new = Arc::new(SnapshotProjectionProvider {
            projected: None,
            requests: Mutex::default(),
        });
        let selected = Arc::new(Mutex::new(old.clone() as Arc<dyn Provider>));
        let resolve = selected.clone();
        let context = Arc::new(CapturedProjectionContext::default());
        let agent = Harness::new(
            store,
            old.clone(),
            ToolRegistry::new(),
            HarnessConfig {
                context: context.clone(),
                provider_resolver: Some(Arc::new(move || {
                    ("test".into(), resolve.lock().unwrap().clone())
                })),
                ..Default::default()
            },
        );
        agent.start(&session.id, "task", RunOptions::default())?;
        context.entered.notified().await;
        *selected.lock().unwrap() = new.clone();
        context.release.notify_one();
        let run = tokio::time::timeout(Duration::from_secs(5), agent.wait_for_idle(&session.id))
            .await??;
        assert_eq!(*context.estimates.lock().unwrap(), vec![projected]);
        assert!(new.requests.lock().unwrap().is_empty());
        if projected <= 100 {
            assert_eq!(run.status, "completed");
            assert_eq!(old.requests.lock().unwrap().len(), 1);
        } else {
            assert_eq!(run.status, "failed");
            assert!(
                run.error
                    .unwrap()
                    .contains("captured provider exceeds fit budget")
            );
            assert!(old.requests.lock().unwrap().is_empty());
        }
    }
    Ok(())
}

#[tokio::test]
async fn manual_compaction_uses_its_captured_provider_across_reload() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("manual bound provider")?;
    let old = Arc::new(SnapshotProjectionProvider {
        projected: Some(80),
        requests: Mutex::default(),
    });
    let new = Arc::new(SnapshotProjectionProvider {
        projected: None,
        requests: Mutex::default(),
    });
    let selected = Arc::new(Mutex::new(old.clone() as Arc<dyn Provider>));
    let resolve = selected.clone();
    let context = Arc::new(CapturedProjectionContext::default());
    let agent = Harness::new(
        store,
        old,
        ToolRegistry::new(),
        HarnessConfig {
            context: context.clone(),
            provider_resolver: Some(Arc::new(move || {
                ("test".into(), resolve.lock().unwrap().clone())
            })),
            ..Default::default()
        },
    );
    let compact = tokio::spawn(async move { agent.compact_session(&session.id, None).await });
    context.entered.notified().await;
    *selected.lock().unwrap() = new;
    context.release.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), compact).await???,
        Some("bound snapshot".into())
    );
    assert_eq!(*context.estimates.lock().unwrap(), vec![80]);
    Ok(())
}

struct LastMessage;
impl ContextPreparation for LastMessage {
    fn project_history(&self, messages: &[Message]) -> Result<Vec<Message>> {
        Ok(messages.last().cloned().into_iter().collect())
    }
}
#[tokio::test]
async fn replaceable_context_hook_keeps_canonical_history_and_hard_limit_is_enforced() -> Result<()>
{
    let store = Store::in_memory()?;
    let session = store.create_session("context")?;
    let provider = Arc::new(Script::new(vec![answer("one"), answer("two")]));
    let agent = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            context: Arc::new(LastMessage),
            ..Default::default()
        },
    );
    agent
        .prompt(&session.id, "a", RunOptions::default())
        .await?;
    agent
        .prompt(&session.id, "b", RunOptions::default())
        .await?;
    assert_eq!(provider.requests.lock().unwrap()[1].messages.len(), 1);
    assert_eq!(store.messages(&session.id)?.len(), 4);
    let limited = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            max_request_bytes: 10,
            ..Default::default()
        },
    );
    let run = limited
        .prompt(&session.id, "c", RunOptions::default())
        .await?;
    assert_eq!(run.status, "failed");
    assert!(run.error.unwrap().contains("request exceeds"));
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert_eq!(store.messages(&session.id)?.len(), 5);
    Ok(())
}

struct PerSession {
    entered: Arc<Notify>,
}
#[async_trait]
impl Provider for PerSession {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        if request.messages[0].content == "wait" {
            self.entered.notify_one();
            std::future::pending().await
        } else {
            Ok(answer("independent"))
        }
    }
}
#[tokio::test]
async fn sessions_run_independently_with_single_active_per_session() -> Result<()> {
    let store = Store::in_memory()?;
    let a = store.create_session("a")?;
    let b = store.create_session("b")?;
    let entered = Arc::new(Notify::new());
    let agent = harness(
        store.clone(),
        Arc::new(PerSession {
            entered: entered.clone(),
        }),
        ToolRegistry::new(),
    );
    agent.start(&a.id, "wait", RunOptions::default())?;
    entered.notified().await;
    assert!(store.fork(&a.id, "busy fork").is_err());
    assert!(
        agent
            .start(&a.id, "duplicate", RunOptions::default())
            .is_err()
    );
    assert_eq!(
        agent
            .prompt(&b.id, "go", RunOptions::default())
            .await?
            .status,
        "completed"
    );
    assert_eq!(store.messages(&a.id)?.len(), 1);
    agent.shutdown(Duration::from_secs(1)).await?;
    Ok(())
}

#[test]
fn workspace_mismatch_is_rejected_and_noop_binding_is_read_only() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    store.bind_workspace(dir.path())?;
    let before = store.storage_stats()?.transaction_count;
    store.bind_workspace(dir.path())?;
    assert_eq!(store.storage_stats()?.transaction_count, before);
    let other = tempfile::tempdir()?;
    assert!(store.bind_workspace(other.path()).is_err());
    Ok(())
}

#[test]
fn clean_reopen_keeps_record_read_only_and_database_owner_lock_lives_until_close() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("archive.sqlite");
    let store = Store::open(&path)?;
    store.create_session("archive")?;
    let other = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(Store::legacy_data_dir(&path).join("store.lock"))?;
    assert!(fs2::FileExt::try_lock_exclusive(&other).is_err());
    drop(store);
    fs2::FileExt::try_lock_exclusive(&other)?;
    fs2::FileExt::unlock(&other)?;
    let reopened = Store::open(&path)?;
    assert_eq!(reopened.storage_stats()?.chat_transaction_count, 0);
    assert_eq!(reopened.storage_stats()?.chat_transaction_count, 0);
    assert_eq!(reopened.list_sessions()?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn fork_retains_complete_history_and_changes_message_ownership() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("source")?;
    let agent = harness(
        store.clone(),
        Arc::new(Script::new(vec![answer("answer")])),
        ToolRegistry::new(),
    );
    agent
        .prompt(&session.id, "question", RunOptions::default())
        .await?;
    let fork = store.fork(&session.id, "fork")?;
    assert_eq!(fork.parent_session_id.as_deref(), Some(session.id.as_str()));
    let messages = store.messages(&fork.id)?;
    assert_eq!(messages.len(), 2);
    assert!(messages.iter().all(|m| m.session_id == fork.id));
    assert_ne!(messages[0].id, store.messages(&session.id)?[0].id);
    Ok(())
}

#[test]
fn context_strategy_roundtrips_and_is_fixed_on_resume_and_fork() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("strategies.sqlite");
    let store = Store::open(&path)?;
    let mut sessions = Vec::new();
    for (strategy, serialized) in [
        (ContextStrategy::Lcm, "lcm"),
        (ContextStrategy::Classical, "classical"),
    ] {
        let session = store.create_session_with_strategy(serialized, strategy)?;
        assert_eq!(
            serde_json::to_value(&session)?["context_strategy"],
            serialized
        );
        let before = store.storage_stats()?.transaction_count;
        store.ensure_context_strategy(&session.id, strategy)?;
        let other = if strategy == ContextStrategy::Lcm {
            ContextStrategy::Classical
        } else {
            ContextStrategy::Lcm
        };
        assert!(store.ensure_context_strategy(&session.id, other).is_err());
        assert_eq!(store.storage_stats()?.transaction_count, before);
        assert_eq!(
            store
                .rename_session(&session.id, "renamed")?
                .context_strategy,
            strategy
        );
        let fork = store.fork(&session.id, "fork")?;
        assert_eq!(fork.context_strategy, strategy);
        assert!(store.context_view(&fork.id)?.is_none());
        assert!(store.context_checkpoint(&fork.id)?.is_none());
        sessions.push(session);
    }
    assert_eq!(
        store.create_session("default")?.context_strategy,
        ContextStrategy::Lcm
    );
    assert!(
        store
            .ensure_context_strategy("missing", ContextStrategy::Lcm)
            .is_err()
    );
    drop(store);
    let reopened = Store::open(&path)?;
    for session in sessions {
        assert_eq!(
            reopened.session(&session.id)?.context_strategy,
            session.context_strategy
        );
    }
    assert_eq!(reopened.storage_stats()?.chat_transaction_count, 0);
    Ok(())
}

#[test]
fn legacy_session_and_checkpoint_default_without_rewriting_archive() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("legacy-policy.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("legacy")?;
    let message = Store::message(&session.id, Role::User, "original".into());
    store.append(&message)?;
    store.save_context_checkpoint(&ContextCheckpoint {
        id: id(),
        session_id: session.id.clone(),
        through_message_id: message.id.clone(),
        model: "old-summary".into(),
        summary: "Legacy summary".into(),
        files_touched: Vec::new(),
        created_at: now(),
        usage: None,
    })?;
    let record_path = store.chat_path(&session.id)?.unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&record_path)?;
    conn.execute(
        "UPDATE sessions SET data=json_remove(data,'$.context_strategy') WHERE id=?1",
        [&session.id],
    )?;
    conn.execute("UPDATE context_checkpoints SET data=json_remove(data,'$.files_touched') WHERE session_id=?1", [&session.id])?;
    let originals: (String, String, String) = conn.query_row(
        "SELECT s.data,c.data,m.data FROM sessions s JOIN context_checkpoints c ON c.session_id=s.id JOIN messages m ON m.session_id=s.id WHERE s.id=?1",
        [&session.id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    )?;
    drop(conn);
    let reopened = Store::open(&path)?;
    assert_eq!(
        reopened.session(&session.id)?.context_strategy,
        ContextStrategy::Lcm
    );
    assert!(
        reopened
            .context_checkpoint(&session.id)?
            .unwrap()
            .files_touched
            .is_empty()
    );
    reopened.ensure_context_strategy(&session.id, ContextStrategy::Lcm)?;
    assert!(
        reopened
            .ensure_context_strategy(&session.id, ContextStrategy::Classical)
            .is_err()
    );
    assert_eq!(reopened.storage_stats()?.chat_transaction_count, 0);
    let conn = rusqlite::Connection::open(&record_path)?;
    let after: (String, String, String) = conn.query_row(
        "SELECT s.data,c.data,m.data FROM sessions s JOIN context_checkpoints c ON c.session_id=s.id JOIN messages m ON m.session_id=s.id WHERE s.id=?1",
        [&session.id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    )?;
    assert_eq!(after, originals);
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?,
        12
    );
    Ok(())
}

#[test]
fn removed_context_strategy_is_rejected_without_rewriting_persisted_sessions() -> Result<()> {
    let error = serde_json::from_str::<ContextStrategy>("\"minimal\"").unwrap_err();
    assert!(error.to_string().contains("unknown variant `minimal`"));
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("removed-strategy.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("removed strategy fixture")?;
    let record_path = store.chat_path(&session.id)?.unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&record_path)?;
    conn.execute(
        "UPDATE sessions SET data=json_set(data,'$.context_strategy','minimal') WHERE id=?1",
        [&session.id],
    )?;
    let original: String = conn.query_row(
        "SELECT data FROM sessions WHERE id=?1",
        [&session.id],
        |row| row.get(0),
    )?;
    drop(conn);
    assert!(
        Store::open(&path)
            .err()
            .unwrap()
            .to_string()
            .contains("unknown variant `minimal`")
    );
    let conn = rusqlite::Connection::open(&record_path)?;
    let after: String = conn.query_row(
        "SELECT data FROM sessions WHERE id=?1",
        [&session.id],
        |row| row.get(0),
    )?;
    assert_eq!(after, original);
    Ok(())
}

#[test]
fn classical_checkpoint_rejects_partial_tool_round_and_persists_file_trail() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session_with_strategy("classical", ContextStrategy::Classical)?;
    let mut assistant = Store::message(&session.id, Role::Assistant, String::new());
    assistant.tool_calls = vec![call("complete")];
    store.append(&assistant)?;
    let mut checkpoint = ContextCheckpoint {
        id: id(),
        session_id: session.id.clone(),
        through_message_id: assistant.id.clone(),
        model: "summary".into(),
        summary: "rolling summary".into(),
        files_touched: vec!["src/main.rs".into()],
        created_at: now(),
        usage: None,
    };
    let before = store.storage_stats()?.transaction_count;
    assert!(store.save_context_checkpoint(&checkpoint).is_err());
    assert_eq!(store.storage_stats()?.transaction_count, before);
    assert!(store.context_checkpoint(&session.id)?.is_none());
    let mut output = Store::message(&session.id, Role::Tool, "complete result".into());
    output.tool_call_id = Some("complete".into());
    store.append(&output)?;
    assert!(store.save_context_checkpoint(&checkpoint).is_err());
    checkpoint.through_message_id = output.id;
    store.save_context_checkpoint(&checkpoint)?;
    assert_eq!(
        store
            .context_checkpoint(&session.id)?
            .unwrap()
            .files_touched,
        vec!["src/main.rs"]
    );
    Ok(())
}

#[path = "tests_context_views.rs"]
mod context_views;
