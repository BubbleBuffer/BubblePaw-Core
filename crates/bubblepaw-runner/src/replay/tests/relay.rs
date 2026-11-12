//! Fallback to Flash and relayed deciders.
use super::*;

#[tokio::test]
async fn unavailable_haiku_summary_falls_back_to_flash_for_summary_probe_and_judge() -> Result<()> {
    use axum::{Json, Router, extract::State, routing::post};
    type Requests = Arc<Mutex<Vec<Value>>>;
    async fn respond(State(requests): State<Requests>, Json(body): Json<Value>) -> Json<Value> {
        let mut requests = requests.lock().unwrap();
        requests.push(body);
        let text = if requests.len() == 3 {
            "{\"grade\":\"supported\",\"reason\":\"Blue is supported.\"}"
        } else {
            "Blue."
        };
        Json(
            json!({"choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}],"usage":{"prompt_tokens":17,"completion_tokens":2}}),
        )
    }
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let app = Router::new()
        .route("/chat/completions", post(respond))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir()?;
    let meter = Arc::new(Mutex::new(Telemetry::default()));
    let mut config = config_for_decider(&format!("llm/{FLASH_MODEL}"));
    config.roles.summary.models = vec![HAIKU_MODEL.into(), FLASH_MODEL.into()];
    config.roles.chapter.models = config.roles.summary.models.clone();
    config.roles.summary.reasoning_effort = ReasoningEffort::High;
    let mut endpoints = ProviderEndpoints {
        opencode_base_url: Some(base),
        opencode_messages_base_url: None,
        ..Default::default()
    };
    let mut keys = ProviderKeys {
        opencode: Some("fake".into()),
        ..Default::default()
    };
    let proxy = relay_configured_keys(
        &mut config,
        &mut endpoints,
        meter.clone(),
        directory.path().join("calls.jsonl"),
        example_prices(),
        &mut keys,
    )
    .await?;
    let mut registry = ProviderRegistry::new(
        "replay",
        Arc::new(ScriptedProvider::new("must not be called")),
    );
    endpoints.register_with_keys(&mut registry, keys)?;
    let registry = Arc::new(registry);
    assert_eq!(config.roles.summary.models, vec![HAIKU_MODEL, FLASH_MODEL]);
    let selected = config
        .roles
        .summary
        .models
        .iter()
        .find(|model| registry.llm_available(model))
        .unwrap()
        .clone();
    assert_eq!(selected, FLASH_MODEL);
    let models = HttpEpisodeModels::from_registry(config.clone(), registry.clone(), None)?;
    meter.lock().unwrap().begin_operation("summary");
    models
        .summarize_classical(ClassicalInput {
            session_id: "local".into(),
            previous_summary: None,
            latest_user_request: None,
            span_text: "Blue facts".into(),
        })
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
        model: selected,
        effort: config.roles.summary.reasoning_effort,
        registry: Some(registry),
    });
    let store = Store::open(directory.path().join("probe.sqlite"))?;
    let session = store.create_session("probe")?;
    let probe: crate::probes::Probe = serde_json::from_value(
        json!({"id":"p","type":"recall","question":"What colour?","expected_answer":"Blue","evidence_message_ids":[],"earliest_checkpoint":1}),
    )?;
    let frozen = ModelRequest {
        messages: vec![Message {
            id: "u1".into(),
            session_id: session.id.clone(),
            role: Role::User,
            origin: None,
            content: "Blue facts".into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_state: Some(
                json!({"adapter":"responses","items":[{"encrypted_content":"opaque encrypted state"}]}),
            ),
            is_error: false,
            created_at: String::new(),
        }],
        ..Default::default()
    };
    let answer = crate::probes::answer(model.clone(), &store, &session.id, &frozen, &probe).await;
    assert_eq!(answer["status"], "answered");
    assert_eq!(
        crate::probes::judge(model.as_ref(), &probe, "Blue.").await["status"],
        "graded"
    );
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        for request in requests.iter() {
            assert_eq!(request["model"], "deepseek-v4.1-flash");
            assert_eq!(request["reasoning_effort"], "high");
            assert!(!request.to_string().contains("opaque encrypted state"));
        }
    }
    assert_eq!(meter.lock().unwrap().haiku_cost_usd, 0.0);
    proxy.abort();
    server.abort();
    Ok(())
}

#[tokio::test]
async fn selected_luna_and_flash_deciders_are_relayed_with_only_their_credentials() -> Result<()> {
    use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
    use std::collections::VecDeque;
    type Replies = Arc<Mutex<VecDeque<(StatusCode, Value)>>>;
    async fn respond(
        State(replies): State<Replies>,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        assert!(body["model"] == "example-decider" || body["model"] == "deepseek-v4.1-flash");
        let (status, payload) = replies.lock().unwrap().pop_front().unwrap();
        (status, Json(payload))
    }
    for luna in [true, false] {
        let payload = if luna {
            json!({"model":"example-decider","answers":[{"name":"choice","type":"choice","choice":"none","probabilities":[{"value":"1","probability":0.0},{"value":"none","probability":1.0}]}],"usage":{"input_tokens":11,"output_tokens":999}})
        } else {
            json!({"choices":[{"message":{"content":"{\"choice\":\"none\"}"}}],"usage":{"prompt_tokens":11,"completion_tokens":2}})
        };
        // Luna is calibrated to one sample per split; Flash keeps three.
        let samples = if luna { 1 } else { 3 };
        let mut replies = VecDeque::from([(StatusCode::TOO_MANY_REQUESTS, json!({}))]);
        replies.extend(std::iter::repeat_n((StatusCode::OK, payload), samples));
        let replies: Replies = Arc::new(Mutex::new(replies));
        let queue = replies.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/decisions", post(respond))
                    .route("/chat/completions", post(respond))
                    .with_state(replies),
            )
            .await
            .unwrap();
        });
        let directory = tempfile::tempdir()?;
        let meter = Arc::new(Mutex::new(Telemetry::default()));
        // Keep Luna first even without its key: metering must follow the
        // model that is actually available, just like split calibration.
        let mut config = EpisodeModelConfig {
            retry_base_delay: Duration::ZERO,
            ..Default::default()
        };
        config.roles.decider.models = vec![
            "openai/example-decider".into(),
            "llm/opencode/deepseek-v4.1-flash".into(),
        ];
        // Calibration for the metered OpenAI decider is explicit here: the
        // production default applies only to the shipped model name.
        config.roles.decider.repeats = luna.then_some(1);
        config.roles.decider.threshold = luna.then_some(0.6);
        let mut endpoints = ProviderEndpoints {
            opencode_base_url: Some(base.clone()),
            openai_base_url: base,
            ..Default::default()
        };
        let mut keys = ProviderKeys {
            opencode: Some("fake-summary".into()),
            openrouter: Some("unused-router-key".into()),
            openai: luna.then(|| "fake-openai".into()),
        };
        let proxy = relay_configured_keys(
            &mut config,
            &mut endpoints,
            meter.clone(),
            directory.path().join("calls.jsonl"),
            example_prices(),
            &mut keys,
        )
        .await?;
        assert!(keys.openrouter.is_none());
        assert_eq!(keys.openai.is_some(), luna);
        assert_eq!(
            config.roles.decider.models,
            vec![if luna {
                "openai/example-decider"
            } else {
                "llm/opencode/deepseek-v4.1-flash"
            }]
        );
        let mut registry = ProviderRegistry::new(
            "replay",
            Arc::new(ScriptedProvider::new("must not be called")),
        );
        endpoints.register_with_keys(&mut registry, keys)?;
        let models = HttpEpisodeModels::from_registry(config, Arc::new(registry), None)?;
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
        let input = 11 * samples as u64;
        assert_eq!(decision.usage.input_tokens, input);
        // Physical usage preserves reported tokens even though Luna's relay
        // billing below prices only input and records zero billable output.
        assert_eq!(decision.usage.output_tokens, if luna { 999 } else { 6 });
        {
            let meter = meter.lock().unwrap();
            assert_eq!(meter.calls.len(), samples + 1);
            assert_eq!(meter.retries, 1);
            assert_eq!(meter.calls[1]["transport_retry"], true);
            assert_eq!(meter.calls[samples]["request_slot"], samples);
            assert_eq!(meter.jev_cost_usd, 0.0);
            if luna {
                assert!((meter.openai_cost_usd - input as f64 * 0.00000010).abs() < 1e-12);
                assert_eq!(meter.flash_cost_usd, 0.0);
                assert_eq!(meter.calls[1]["output_tokens"], 999);
            } else {
                assert!(
                    (meter.flash_cost_usd - (33.0 * 0.0000003 + 6.0 * 0.0000012)).abs() < 1e-12
                );
                assert_eq!(meter.openai_cost_usd, 0.0);
            }
        }
        assert!(queue.lock().unwrap().is_empty());
        proxy.abort();
        server.abort();
    }
    Ok(())
}
