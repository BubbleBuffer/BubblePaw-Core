//! Shared policy and effect boundaries across native composition routes.
use super::*;

#[tokio::test]
async fn direct_host_call_is_correctable_and_never_crosses_effect_boundary() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("archive.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("model entrypoints")?;
    let native = Arc::new(Native {
        contexts: Mutex::default(),
        result: "native completed".into(),
    });
    let mut tools = ToolRegistry::new();
    let reads = Arc::new(AtomicUsize::new(0));
    tools.register_host(native.clone())?;
    tools.register_host(Arc::new(Guarded(reads.clone())))?;
    tools.register(Arc::new(HostBridge))?;
    let provider = Arc::new(Script::new(vec![
        ModelResponse {
            tool_calls: vec![call("rejected-native")],
            ..Default::default()
        },
        bridge_call(
            "accepted-compose",
            "guarded",
            serde_json::json!({"count":1}),
        ),
        answer("corrected"),
    ]));
    let records = Arc::new(RecordedObservations::default());
    let agent = Harness::new(
        store.clone(),
        provider.clone(),
        tools,
        HarnessConfig {
            execution_mode: ExecutionMode::Native,
            observation: Some(Observation {
                sink: records.clone(),
                mode: ObservationMode::Metadata,
                max_payload_bytes: 1024,
            }),
            ..Default::default()
        },
    );
    let run = tokio::time::timeout(
        Duration::from_secs(10),
        agent.prompt(
            &session.id,
            "start",
            RunOptions {
                tool_names: Some(vec!["compose".into(), "guarded".into()]),
                ..Default::default()
            },
        ),
    )
    .await??;
    assert_eq!(run.status, "completed");
    assert!(native.contexts.lock().unwrap().is_empty());
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    let requests = provider.requests.lock().unwrap();
    let frozen = serde_json::to_vec(&requests[0].tools)?;
    assert!(
        requests
            .iter()
            .all(|request| serde_json::to_vec(&request.tools).unwrap() == frozen)
    );
    let rejection = requests[1]
        .messages
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some("rejected-native"))
        .unwrap();
    assert!(rejection.is_error);
    assert_eq!(
        rejection.content,
        "Tool `native` is not available in this run. Allowed tools: compose, guarded."
    );
    drop(requests);
    let conn = rusqlite::Connection::open(store.chat_path(&session.id)?.unwrap())?;
    let rejected_effects: i64 = conn.query_row(
        "SELECT COUNT(*) FROM effects WHERE call_id='rejected-native'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(rejected_effects, 0);
    let effects: i64 = conn.query_row("SELECT COUNT(*) FROM effects", [], |row| row.get(0))?;
    assert_eq!(
        effects, 1,
        "only the outer entrypoint is journaled; the nested host read has no effect"
    );
    assert!(store.pending_effects(&session.id)?.is_empty());
    let records = records.0.lock().unwrap();
    let rejected: Vec<_> = records
        .iter()
        .filter(|record| record.scope.call_id.as_deref() == Some("rejected-native"))
        .collect();
    assert_eq!(rejected.len(), 2);
    assert_eq!(rejected[0].kind, "tool_started");
    assert_eq!(rejected[1].kind, "tool_finished");
    assert_eq!(rejected[1].metrics["is_error"], true);
    assert!(
        rejected
            .iter()
            .all(|record| record.scope.invocation_id.is_none())
    );
    Ok(())
}

#[tokio::test]
async fn host_policy_survives_model_selection_and_denies_disallowed_nested_calls() -> Result<()> {
    for read_only in [false, true] {
        let store = Store::in_memory()?;
        let session = store.create_session("restricted hosts")?;
        let native = Arc::new(Native {
            contexts: Mutex::default(),
            result: "denied".into(),
        });
        let reads = Arc::new(AtomicUsize::new(0));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(HostBridge))?;
        tools.register_host(native.clone())?;
        tools.register_host(Arc::new(Guarded(reads.clone())))?;
        let tools = if read_only {
            tools.read_only()
        } else {
            tools.restricted_capabilities(&["guarded".into()])
        };
        let provider = Arc::new(Script::new(vec![
            bridge_call(
                "denied-native",
                "native",
                serde_json::json!({"value":"host value"}),
            ),
            bridge_call("allowed-read", "guarded", serde_json::json!({"count":1})),
            ModelResponse {
                tool_calls: vec![ToolCall {
                    id: "allowed-direct".into(),
                    name: "guarded".into(),
                    arguments: serde_json::json!({"count":1}),
                }],
                ..Default::default()
            },
            ModelResponse {
                tool_calls: vec![ToolCall {
                    id: "allowed-wrapper".into(),
                    name: "call_tool".into(),
                    arguments: serde_json::json!({"name":"guarded","arguments":{"count":1}}),
                }],
                ..Default::default()
            },
            answer("done"),
        ]));
        let agent = harness(store.clone(), provider.clone(), tools);
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(10),
                agent.prompt(
                    &session.id,
                    "start",
                    RunOptions {
                        tool_names: Some(vec!["compose".into(), "guarded".into()]),
                        ..Default::default()
                    }
                )
            )
            .await??
            .status,
            "completed"
        );
        assert!(native.contexts.lock().unwrap().is_empty());
        assert_eq!(reads.load(Ordering::Relaxed), 3);
        let messages = store.messages(&session.id)?;
        let denied = messages
            .iter()
            .find(|message| message.tool_call_id.as_deref() == Some("denied-native"))
            .unwrap();
        assert!(denied.is_error);
        assert_eq!(denied.content, "Unknown tool: native");
        for call in ["allowed-read", "allowed-direct", "allowed-wrapper"] {
            let result = messages
                .iter()
                .find(|message| message.tool_call_id.as_deref() == Some(call))
                .unwrap();
            assert!(!result.is_error, "{call}: {}", result.content);
        }
        let requests = provider.requests.lock().unwrap();
        let frozen = serde_json::to_vec(&requests[0].tools)?;
        assert!(
            requests
                .iter()
                .all(|request| serde_json::to_vec(&request.tools).unwrap() == frozen)
        );
        assert!(!requests[0].tools.iter().any(|tool| tool.name == "native"));
        assert!(store.pending_effects(&session.id)?.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn shared_dispatch_validates_nested_reads_and_keeps_only_explicit_outputs_durable()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("archive.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("composition")?;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register_host(Arc::new(Guarded(calls.clone())))?;
    tools.register(Arc::new(Composite))?;
    assert_eq!(tools.definitions().len(), 1);
    assert_eq!(tools.capabilities().len(), 1);
    let provider = Arc::new(Script::new(vec![
        ModelResponse {
            tool_calls: vec![ToolCall {
                id: "outer-call".into(),
                name: "composite".into(),
                arguments: serde_json::json!({}),
            }],
            ..Default::default()
        },
        answer("done"),
    ]));
    let harness = harness(store.clone(), provider.clone(), tools);
    let run = tokio::time::timeout(
        Duration::from_secs(10),
        harness.prompt(&session.id, "start", RunOptions::default()),
    )
    .await??;
    assert_eq!(run.status, "completed");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(store.messages(&session.id)?.len(), 4);
    assert!(!serde_json::to_string(&store.messages(&session.id)?)?.contains("nested read payload"));
    assert_eq!(
        store.outputs(&session.id, 10)?[0].output["text"],
        "published"
    );
    assert!(store.pending_effects(&session.id)?.is_empty());
    drop(harness);
    drop(store);
    let reopened = Store::open(&path)?;
    assert_eq!(
        reopened.outputs(&session.id, 10)?[0].output["text"],
        "published"
    );
    Ok(())
}
