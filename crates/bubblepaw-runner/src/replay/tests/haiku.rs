//! Replay pricing and the Haiku roles.
use super::*;

#[test]
fn unpriced_replay_models_are_rejected_before_relay_dispatch() {
    let mut unknown_summary = EpisodeModelConfig::default();
    unknown_summary.roles.summary.models = vec!["custom/other-model".into()];
    for config in [
        unknown_summary,
        config_for_decider("openai/unknown-model"),
        config_for_decider("llm/custom/model"),
    ] {
        assert!(replay_roles(&config, example_prices(), true).is_err());
    }
}

#[test]
fn haiku_prices_whole_prompt_threshold_and_cache_subsets() {
    let prices = example_prices();
    let mut usage = Usage {
        input_tokens: 100_000,
        output_tokens: 2_000,
        cache_read_tokens: 60_000,
        cache_creation_tokens: 20_000,
        reasoning_tokens: None,
    };
    let cost = |usage: &Usage| prices.usage_cost(HAIKU_MODEL, usage, None);
    assert!((cost(&usage).unwrap() - 0.0061).abs() < 1e-12);
    usage.input_tokens += 1;
    assert!((cost(&usage).unwrap() - 0.0305005).abs() < 1e-12);
    assert!(
        prices
            .reservation(HAIKU_MODEL, usage.input_tokens, usage.output_tokens)
            .unwrap()
            > cost(&usage).unwrap()
    );
    usage.cache_creation_tokens = 100_000;
    assert!(cost(&usage).is_none());
    // Model names match exactly after the provider prefix, so these are unpriced.
    let small = Usage {
        input_tokens: 1,
        output_tokens: 1,
        ..Default::default()
    };
    assert!(
        prices
            .usage_cost("opencode-messages/claude-haiku-5-5-extra", &small, None)
            .is_none()
    );
    assert!(prices.is_priced(&format!("llm/{HAIKU_MODEL}")));
    let mut config = config_for_decider(&format!("llm/{HAIKU_MODEL}"));
    config.roles.summary.models = vec![HAIKU_MODEL.into(), FLASH_MODEL.into()];
    config.roles.chapter.models = vec![HAIKU_MODEL.into()];
    assert!(replay_roles(&config, prices, true).is_ok());
    config
        .roles
        .summary
        .models
        .push("custom/unpriced-fallback".into());
    assert!(replay_roles(&config, prices, true).is_err());
    // Summary and chapter roles accept only the exact configured ids.
    config.roles.summary.models = vec!["opencode/claude-haiku-5-5".into()];
    assert!(replay_roles(&config, prices, true).is_err());
    let mut meter = Telemetry {
        max_cost_usd: 0.10,
        haiku_cost_usd: 0.04,
        reserved_unknown_cost_usd: 0.02,
        ..Default::default()
    };
    assert!(meter.reserve_next(0.03));
    assert!(!meter.reserve_next(prices.reservation(HAIKU_MODEL, 100_001, 65_536).unwrap()));
}

#[tokio::test]
async fn selected_haiku_roles_probes_and_judge_use_messages_and_cache_aware_metering() -> Result<()>
{
    use crate::probes::ProbeModel;
    use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
    type Requests = Arc<Mutex<Vec<Value>>>;
    async fn respond(
        State(requests): State<Requests>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        assert_eq!(headers["x-api-key"], "fake-messages");
        assert_eq!(headers["anthropic-version"], "2023-06-01");
        let mut requests = requests.lock().unwrap();
        requests.push(body);
        let index = requests.len();
        let (content, stop) = match index {
            1 => (
                json!([{"type":"text","text":"# Episode\nBlue facts."}]),
                "end_turn",
            ),
            2 => (
                json!([{"type":"text","text":"Classical blue facts."}]),
                "end_turn",
            ),
            3 => (
                json!([{"type":"text","text":"# Chapter\nBlue facts.\nRelated: none"}]),
                "end_turn",
            ),
            4 => (
                json!([{"type":"thinking","thinking":"Look up the original","signature":"opaque-signature"},{"type":"tool_use","id":"haiku-search","name":"history_search","input":{"query":"blue","limit":1}}]),
                "tool_use",
            ),
            5 => (json!([{"type":"text","text":"Blue."}]), "end_turn"),
            6 => (
                json!([{"type":"text","text":"{\"grade\":\"supported\",\"reason\":\"Blue is supported.\"}"}]),
                "end_turn",
            ),
            7 => (
                json!([{"type":"text","text":"{\"choice\":\"none\"}"}]),
                "end_turn",
            ),
            _ => {
                return Json(
                    json!({"content":[{"type":"text","text":"Unknown usage"}],"stop_reason":"end_turn"}),
                );
            }
        };
        // Whole prompt is 100001 although uncached input alone is 20001.
        Json(
            json!({"model":"claude-haiku-5-5","content":content,"stop_reason":stop,"usage":{"input_tokens":20_001,"cache_read_input_tokens":60_000,"cache_creation_input_tokens":20_000,"output_tokens":2_000}}),
        )
    }
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let app = Router::new()
        .route("/messages", post(respond))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir()?;
    let meter = Arc::new(Mutex::new(Telemetry {
        max_cost_usd: 5.0,
        ..Default::default()
    }));
    let mut config = config_for_decider(&format!("llm/{HAIKU_MODEL}"));
    config.roles.summary.models = vec![HAIKU_MODEL.into()];
    config.roles.summary.reasoning_effort = ReasoningEffort::High;
    config.roles.chapter.models = vec![HAIKU_MODEL.into()];
    config.roles.chapter.reasoning_effort = ReasoningEffort::Medium;
    config.roles.decider.reasoning_effort = ReasoningEffort::Medium;
    config.roles.decider.repeats = Some(1);
    let mut endpoints = ProviderEndpoints {
        opencode_base_url: None,
        opencode_messages_base_url: Some(base),
        ..Default::default()
    };
    let mut keys = ProviderKeys {
        opencode: Some("fake-messages".into()),
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
    assert_eq!(config.roles.summary.models, vec![HAIKU_MODEL]);
    let mut registry = ProviderRegistry::new(
        "replay",
        Arc::new(ScriptedProvider::new("must not be called")),
    );
    endpoints.register_with_keys(&mut registry, keys)?;
    let registry = Arc::new(registry);
    let models = HttpEpisodeModels::from_registry(config.clone(), registry.clone(), None)?;
    meter.lock().unwrap().begin_operation("summary");
    let summary = models
        .summarize(SummaryInput {
            focus: None,
            session_id: "local".into(),
            previous_summary: None,
            turn_request: None,
            episode_text: "Blue facts.".into(),
            cap: 100,
        })
        .await?;
    assert_eq!(summary.usage.input_tokens, 100_001);
    assert_eq!(summary.usage.cache_read_tokens, 60_000);
    assert_eq!(summary.usage.cache_creation_tokens, 20_000);
    let aggregate_usage = summary.usage.clone();
    meter.lock().unwrap().begin_operation("summary");
    models
        .summarize_classical(ClassicalInput {
            session_id: "local".into(),
            previous_summary: None,
            latest_user_request: None,
            span_text: "Blue facts.".into(),
        })
        .await?;
    meter.lock().unwrap().begin_operation("chapter");
    models
        .close_chapter(ChapterInput {
            session_id: "local".into(),
            episodes: vec![summary.summary],
            previous_chapters: vec![],
            typed_user_texts: vec![],
            task_instructions: Vec::new(),
        })
        .await?;
    let probe_model = Arc::new(ReplayProbeModel {
        prices: example_prices(),
        context_route: Default::default(),
        telemetry: meter.clone(),
        url: None,
        client: reqwest::Client::new(),
        model: HAIKU_MODEL.into(),
        effort: ReasoningEffort::High,
        registry: Some(registry),
    });
    let store = Store::open(directory.path().join("probe.sqlite"))?;
    let session = store.create_session("probe")?;
    let source = Message {
        id: "persisted-source".into(),
        session_id: session.id.clone(),
        role: Role::User,
        origin: None,
        content: "Blue facts.".into(),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: String::new(),
    };
    store.append_batch(std::slice::from_ref(&source))?;
    store.save_context_checkpoint(&bubblepaw_core::ContextCheckpoint {
        id: "cache-usage-proof".into(),
        session_id: session.id.clone(),
        through_message_id: source.id,
        model: HAIKU_MODEL.into(),
        summary: "Aggregated episode summary.".into(),
        files_touched: vec![],
        created_at: String::new(),
        usage: Some(aggregate_usage),
    })?;
    let reopened = Store::open(directory.path().join("probe.sqlite"))?;
    let persisted = reopened
        .context_checkpoint(&session.id)?
        .unwrap()
        .usage
        .unwrap();
    assert_eq!(persisted.input_tokens, 100_001);
    assert_eq!(persisted.output_tokens, 2_000);
    assert_eq!(persisted.cache_read_tokens, 60_000);
    assert_eq!(persisted.cache_creation_tokens, 20_000);
    let probe: crate::probes::Probe = serde_json::from_value(
        json!({"id":"p","type":"recall","question":"What colour?","expected_answer":"Blue","evidence_message_ids":[],"earliest_checkpoint":1}),
    )?;
    let answer = crate::probes::answer(
        probe_model.clone(),
        &store,
        &session.id,
        &ModelRequest::default(),
        &probe,
    )
    .await;
    assert_eq!(answer["status"], "answered");
    assert_eq!(answer["tool_calls"].as_array().unwrap().len(), 1);
    let judgment = crate::probes::judge(probe_model.as_ref(), &probe, "Blue.").await;
    assert_eq!(judgment["status"], "graded");
    meter.lock().unwrap().begin_operation("decision");
    models
        .decide(SplitInput {
            session_id: "local".into(),
            open_chapter: "Blue".into(),
            options: vec![crate::episode_models::SplitOption {
                message_id: "u1".into(),
                user: "continue".into(),
                agent_before: String::new(),
            }],
        })
        .await?;
    {
        let recorded = requests.lock().unwrap();
        assert_eq!(recorded.len(), 7);
        for (index, request) in recorded.iter().enumerate() {
            assert_eq!(request["model"], "claude-haiku-5-5");
            assert_eq!(request["thinking"]["type"], "adaptive");
            assert_eq!(
                request["output_config"]["effort"],
                if matches!(index, 2 | 6) {
                    "medium"
                } else {
                    "high"
                }
            );
        }
        assert_eq!(
            recorded[3]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["call_tool", "history_read", "history_search", "tool_search"]
        );
        assert_eq!(
            recorded[4]["messages"][1]["content"][0]["signature"],
            "opaque-signature"
        );
        assert_eq!(
            recorded[4]["messages"][2]["content"][0]["type"],
            "tool_result"
        );
        assert!(recorded[5]["tools"].is_null());
    }
    // The maximum output reservation also covers this synthetic long-prompt usage.
    {
        let meter = meter.lock().unwrap();
        assert!((meter.haiku_cost_usd - 7.0 * 0.0305005).abs() < 1e-12);
        assert_eq!(meter.flash_cost_usd, 0.0);
        assert_eq!(meter.calls[0]["input_tokens"], 100_001);
        assert_eq!(meter.calls[0]["cache_creation_tokens"], 20_000);
        assert!(meter.stopped.is_none());
    }
    probe_model
        .call(
            json!({"messages":[{"role":"user","content":"uncertain"}],"max_tokens":65536}),
            "probe",
        )
        .await?;
    assert!(meter.lock().unwrap().reserved_unknown_cost_usd > 0.0);
    proxy.abort();
    server.abort();
    Ok(())
}
