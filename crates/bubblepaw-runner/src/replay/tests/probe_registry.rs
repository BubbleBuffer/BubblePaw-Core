//! Probe registry namespaces, history reads and metering.
use super::*;

#[tokio::test]
async fn probe_registry_preserves_worker_namespace_cursor_and_closes_its_mirror() -> Result<()> {
    struct Witness {
        requests: Mutex<Vec<ModelRequest>>,
        closed: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl Provider for Witness {
        async fn generate(
            &self,
            request: ModelRequest,
            _: bubblepaw_core::TextCallback,
            _: CancellationToken,
        ) -> Result<bubblepaw_core::ModelResponse> {
            self.requests.lock().unwrap().push(request);
            Ok(bubblepaw_core::ModelResponse {
                text: "Blue.".into(),
                provider_state: Some(
                    json!({"adapter":"chat-completions","reasoning_content":"opaque worker reasoning"}),
                ),
                ..Default::default()
            })
        }
        async fn close_session(&self, session_id: &str) -> Result<()> {
            self.closed.lock().unwrap().push(session_id.into());
            Ok(())
        }
    }
    let witness = Arc::new(Witness {
        requests: Mutex::new(Vec::new()),
        closed: Mutex::new(Vec::new()),
    });
    let registry = Arc::new(ProviderRegistry::new("opencode", witness.clone()));
    let model = Arc::new(ReplayProbeModel {
        prices: example_prices(),
        context_route: Default::default(),
        telemetry: Arc::new(Mutex::new(Telemetry::default())),
        url: None,
        client: reqwest::Client::new(),
        model: FLASH_MODEL.into(),
        effort: ReasoningEffort::High,
        registry: Some(registry),
    });
    let store = Store::in_memory()?;
    let source = store.create_session("probe source")?;
    let probe: crate::probes::Probe = serde_json::from_value(
        json!({"id":"p","type":"recall","question":"What colour?","expected_answer":"Blue.","evidence_message_ids":[],"earliest_checkpoint":1}),
    )?;
    let answer =
        crate::probes::answer(model, &store, &source.id, &ModelRequest::default(), &probe).await;
    assert_eq!(answer["status"], "answered");
    let child = answer.worker_session_id().unwrap().to_owned();
    {
        let requests = witness.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].session_id, child);
        assert!(!requests[0].history.epoch.is_empty());
        assert_eq!(requests[0].reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(
            requests[0].messages.last().unwrap().origin,
            Some(MessageOrigin::ChildTask)
        );
    }
    assert_eq!(
        witness.closed.lock().unwrap().as_slice(),
        std::slice::from_ref(&child)
    );
    assert_eq!(
        store
            .messages(&child)?
            .last()
            .unwrap()
            .provider_state
            .as_ref()
            .unwrap()["reasoning_content"],
        "opaque worker reasoning"
    );
    answer.acknowledge().await?;
    assert_eq!(
        witness.closed.lock().unwrap().as_slice(),
        std::slice::from_ref(&child)
    );
    Ok(())
}

#[tokio::test]
async fn scripted_probe_reads_history_even_when_frozen_view_has_recorded_tool_results() -> Result<()>
{
    let store = Store::in_memory()?;
    let session = store.create_session("test")?;
    let probe = crate::probes::Probe {
        id: "p1".into(),
        kind: crate::probes::ProbeType::Recall,
        question: "What happened?".into(),
        expected_answer: "It worked".into(),
        evidence_message_ids: vec!["r1".into()],
        earliest_checkpoint: 1,
        ask_at_turn: None,
        origin: None,
        original_question: None,
        source_message_id: None,
        source_turn: None,
    };
    let mut frozen = ModelRequest::default();
    frozen.messages.push(Message {
        id: "r1".into(),
        session_id: session.id.clone(),
        role: Role::Tool,
        content: "Original recorded tool output".into(),
        tool_call_id: Some("recorded-tool".into()),
        tool_calls: vec![],
        origin: None,
        provider_state: None,
        is_error: false,
        created_at: String::new(),
    });
    let model = Arc::new(ReplayProbeModel {
        prices: example_prices(),
        context_route: Default::default(),
        telemetry: Arc::new(Mutex::new(Telemetry::default())),
        url: None,
        client: reqwest::Client::new(),
        model: FLASH_MODEL.into(),
        effort: ReasoningEffort::Low,
        registry: None,
    });
    let result = crate::probes::answer(model.clone(), &store, &session.id, &frozen, &probe).await;
    assert_eq!(result["status"], "answered");
    assert_eq!(result["calls"].as_array().unwrap().len(), 2);
    assert_eq!(result["tool_calls"].as_array().unwrap().len(), 1);
    assert_eq!(result["tool_calls"][0]["name"], "history_search");
    Ok(())
}

#[tokio::test]
async fn registry_calls_preserve_replay_attempt_usage_and_cost_metering() -> Result<()> {
    use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
    use std::collections::VecDeque;
    type Replies = Arc<Mutex<VecDeque<(StatusCode, Value)>>>;
    async fn respond(
        State(replies): State<Replies>,
        Json(_body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        let (status, payload) = replies.lock().unwrap().pop_front().unwrap();
        (status, Json(payload))
    }
    let replies: Replies = Arc::new(Mutex::new(VecDeque::from([
        (StatusCode::BAD_GATEWAY, json!({})),
        (
            StatusCode::OK,
            json!({"choices":[{"message":{"content":"# Summary\nFacts"}}],"usage":{"prompt_tokens":17,"completion_tokens":4,"completion_tokens_details":{"reasoning_tokens":2}}}),
        ),
        (
            StatusCode::OK,
            json!({"model":"example/decider-model","answers":{"choice":{"type":"choice","choice":"none","probabilities":{"1":0.0,"none":1.0}}},"usage":{"input_tokens":11,"output_tokens":2,"cost":0.001}}),
        ),
        (
            StatusCode::OK,
            json!({"model":"example/decider-model","answers":{"choice":{"type":"choice","choice":"none","probabilities":{"1":0.0,"none":1.0}}},"usage":{"input_tokens":11,"output_tokens":2,"cost":0.001}}),
        ),
        (
            StatusCode::OK,
            json!({"model":"example/decider-model","answers":{"choice":{"type":"choice","choice":"none","probabilities":{"1":0.0,"none":1.0}}},"usage":{"input_tokens":11,"output_tokens":2,"cost":0.001}}),
        ),
    ])));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/chat/completions", post(respond))
                .route("/decision", post(respond))
                .with_state(replies),
        )
        .await
        .unwrap();
    });
    let directory = tempfile::tempdir()?;
    let ledger = directory.path().join("calls.jsonl");
    let meter = Arc::new(Mutex::new(Telemetry::default()));
    let mut config = config_for_decider("openrouter/example/decider-model");
    let mut endpoints = ProviderEndpoints {
        opencode_base_url: Some(base.clone()),
        openrouter_decisions_url: format!("{base}/decision"),
        ..Default::default()
    };
    let mut keys = ProviderKeys {
        opencode: Some("fake-summary".into()),
        openrouter: Some("fake-decision".into()),
        openai: None,
    };
    let proxy = relay_configured_keys(
        &mut config,
        &mut endpoints,
        meter.clone(),
        ledger.clone(),
        example_prices(),
        &mut keys,
    )
    .await?;
    let mut registry = ProviderRegistry::new("replay", Arc::new(ScriptedProvider::new("unused")));
    endpoints.register_with_keys(&mut registry, keys)?;
    let models = HttpEpisodeModels::from_registry(config, Arc::new(registry), None)?;
    meter.lock().unwrap().begin_operation("summary");
    let summary = models
        .summarize(SummaryInput {
            focus: None,
            session_id: "local".into(),
            previous_summary: None,
            turn_request: None,
            episode_text: "facts".into(),
            cap: 100,
        })
        .await?;
    assert_eq!(summary.usage.input_tokens, 17);
    meter.lock().unwrap().begin_operation("decision");
    let decision = models
        .decide(SplitInput {
            session_id: "local".into(),
            open_chapter: "facts".into(),
            options: vec![crate::episode_models::SplitOption {
                message_id: "u1".into(),
                user: "continue".into(),
                agent_before: String::new(),
            }],
        })
        .await?;
    assert_eq!(decision.choice, None);
    assert_eq!(decision.usage.input_tokens, 33);
    {
        let meter = meter.lock().unwrap();
        assert_eq!(meter.calls.len(), 5);
        assert_eq!(meter.retries, 1);
        assert_eq!(meter.failures, 1);
        assert_eq!(meter.calls[0]["outcome"], "error");
        assert_eq!(meter.calls[1]["transport_retry"], true);
        assert_eq!(meter.calls[1]["reasoning_tokens"], 2);
        assert_eq!(meter.calls[4]["request_slot"], 3);
        assert!((meter.flash_cost_usd - (17.0 * 0.0000003 + 4.0 * 0.0000012)).abs() < 1e-12);
        assert!((meter.jev_cost_usd - 0.003).abs() < 1e-12);
        assert!(meter.reserved_unknown_cost_usd > 0.0);
    }
    assert_eq!(std::fs::read_to_string(ledger)?.lines().count(), 5);
    proxy.abort();
    server.abort();
    Ok(())
}
