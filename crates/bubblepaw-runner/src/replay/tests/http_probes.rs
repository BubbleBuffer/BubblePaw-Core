use super::*;

#[tokio::test]
async fn local_http_probes_use_metered_history_tools_record_bad_judge_and_block_before_spend()
-> Result<()> {
    use crate::probes::ProbeModel;
    use axum::{Json, Router, extract::State, routing::post};
    type Requests = Arc<Mutex<Vec<Value>>>;
    async fn respond(State(requests): State<Requests>, Json(body): Json<Value>) -> Json<Value> {
        let mut requests = requests.lock().unwrap();
        requests.push(body.clone());
        let message = match requests.len() {
            1 => {
                json!({"role":"assistant","content":null,"tool_calls":[{"id":"read-original","type":"function","function":{"name":"history_read","arguments":"{\"message_id\":\"r1\"}"}}]})
            }
            2 => json!({"role":"assistant","content":"Blue ".repeat(20_000)}),
            _ => json!({"role":"assistant","content":"malformed judge"}),
        };
        Json(
            json!({"choices":[{"message":message,"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":20}}),
        )
    }
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let app = Router::new()
        .route("/chat/completions", post(respond))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("test.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("fixture")?;
    let original = Message {
        id: "r1".into(),
        session_id: session.id.clone(),
        role: Role::User,
        origin: None,
        content: "Garden was blue".into(),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: String::new(),
    };
    store.append_batch(std::slice::from_ref(&original))?;
    let before = serde_json::to_value(store.snapshot(&session.id, 100, 100, 100)?)?;
    let meter = Arc::new(Mutex::new(Telemetry::default()));
    let mut config = config_for_decider("llm/opencode/deepseek-v4.1-flash");
    let mut endpoints = ProviderEndpoints {
        opencode_base_url: Some(base),
        ..Default::default()
    };
    let mut keys = ProviderKeys {
        opencode: Some("fake".into()),
        ..Default::default()
    };
    let relay = relay_configured_keys(
        &mut config,
        &mut endpoints,
        meter.clone(),
        directory.path().join("calls.jsonl"),
        example_prices(),
        &mut keys,
    )
    .await?;
    let model = Arc::new(ReplayProbeModel {
        prices: example_prices(),
        context_route: Default::default(),
        telemetry: meter.clone(),
        url: Some(format!(
            "{}/chat/completions",
            endpoints.opencode_base_url.unwrap()
        )),
        client: reqwest::Client::new(),
        model: FLASH_MODEL.into(),
        effort: ReasoningEffort::Low,
        registry: None,
    });
    let probe = crate::probes::Probe {
        id: "test".into(),
        kind: crate::probes::ProbeType::Recall,
        question: "What colour?".into(),
        expected_answer: "Blue".into(),
        evidence_message_ids: vec!["r1".into()],
        earliest_checkpoint: 1,
        ask_at_turn: None,
        origin: None,
        original_question: None,
        source_message_id: None,
        source_turn: None,
    };
    let frozen = ModelRequest {
        messages: vec![original],
        ..Default::default()
    };
    let result = crate::probes::answer(model.clone(), &store, &session.id, &frozen, &probe).await;
    assert_eq!(result["status"], "answered");
    assert_eq!(result["answer"].as_str().unwrap().len(), 100_000);
    let grade =
        crate::probes::judge(model.as_ref(), &probe, result["answer"].as_str().unwrap()).await;
    assert_eq!(grade["status"], "judge_failed");
    assert!(grade["judgment"].is_null());
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        for request in requests.iter() {
            assert!(request.get("_bubblepaw_request").is_none());
            for message in request["messages"].as_array().unwrap() {
                for key in ["id", "session_id", "origin", "is_error"] {
                    assert!(message.get(key).is_none());
                }
            }
        }
        assert_eq!(
            requests[0]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["function"]["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["call_tool", "history_read", "history_search", "tool_search"]
        );
        assert!(requests[1]["messages"].as_array().unwrap().iter().any(|m| {
            m["tool_call_id"] == "read-original"
                && m["content"].as_str().unwrap().contains("Garden was blue")
        }));
        assert!(requests[2]["tools"].is_null());
        assert_eq!(requests[0]["max_tokens"], crate::probes::MAX_OUTPUT_TOKENS);
    }
    let mut after = store.snapshot(&session.id, 100, 100, 100)?;
    let before_revision = before["revision"].as_u64().unwrap();
    // The probe's canonical worker advances the global revision while
    // preserving every source-session field in this snapshot.
    assert!(after.revision > before_revision);
    after.revision = before_revision;
    assert_eq!(serde_json::to_value(after)?, before);
    {
        let mut meter = meter.lock().unwrap();
        meter.max_cost_usd = 0.05;
    }
    assert!(
        model
            .call(
                json!({"model":"deepseek-v4.1-flash","messages":[],"max_tokens":65536}),
                "probe"
            )
            .await
            .is_err()
    );
    assert_eq!(requests.lock().unwrap().len(), 3);
    assert_eq!(meter.lock().unwrap().calls.len(), 3);
    relay.abort();
    server.abort();
    Ok(())
}
