//! Decider configuration: the built-in chain is gone, so a configured route
//! must be used with its own calibration and a missing one fails at use.
use super::*;

#[tokio::test]
async fn missing_summary_adapter_uses_existing_provider_without_tools() {
    let provider = Arc::new(MainFake {
        requests: Mutex::new(Vec::new()),
    });
    let registry = ProviderRegistry::new("main", provider.clone());
    let mut roles = ModelRoles::for_models("missing/summary".into(), "main/main-model".into());
    roles.decider.models = vec![
        "openai/gpt-6-luna".into(),
        "llm/missing/summary".into(),
        "llm/main/main-model".into(),
    ];
    let models = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles,
            ..Default::default()
        },
        Arc::new(registry),
        None,
    )
    .unwrap();
    let summary = models.summarize(summary()).await.unwrap();
    assert_eq!(summary.model, "main-model");
    assert_eq!(summary.reasoning.as_deref(), Some("main trace"));
    let split = models.decide(split()).await.unwrap();
    assert_eq!(split.model, "main-model");
    assert_eq!(split.choice.as_deref(), Some("u2"));
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert!(
        requests
            .iter()
            .all(|request| request.tools.is_empty() && request.model == "main-model")
    );
}
#[tokio::test]
async fn trace_records_parse_failure_without_keys_or_payload() {
    let server = serve(vec![chat("garbage"); 3]).await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trace.jsonl");
    let trace = JsonlTrace::open(TraceConfig {
        path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    let mut registry = ProviderRegistry::new("main", Arc::new(NoProvider));
    registry.insert("opencode", server.clone()).unwrap();
    let mut config = EpisodeModelConfig::default();
    config.roles.decider.models = vec![
        "openai/gpt-6-luna".into(),
        "llm/opencode/deepseek-v4.1-flash".into(),
    ];
    let models =
        HttpEpisodeModels::from_registry(config, Arc::new(registry), Some(trace.clone())).unwrap();
    assert_eq!(models.decide(split()).await.unwrap().choice, None);
    trace.flush(Duration::from_secs(2)).unwrap();
    let text = std::fs::read_to_string(path).unwrap();
    assert!(!text.contains("never-log-key"));
    assert!(!text.contains("Authorization"));
    assert!(!text.contains("NEXT USER"));
    let calls: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|record| record.to_string().contains("episode_model_call"))
        .collect();
    assert_eq!(calls.len(), 3);
    assert!(
        calls
            .iter()
            .all(|record| record.to_string().contains("invalid_chat_none"))
    );
    let calibration = text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|record| record.to_string().contains("episode_decision_calibration"))
        .unwrap();
    let serialized = calibration.to_string();
    assert!(serialized.contains("llm/opencode/deepseek-v4.1-flash"));
    assert!(serialized.contains("\"repeats\":3"));
    assert!(serialized.contains("\"threshold\":0.8"));
}
/// A Chat route with a wire shape whose cache reads 900 of 1000 tokens after the first call.
#[derive(Default)]
struct ShapedChat {
    calls: std::sync::atomic::AtomicU64,
}
#[async_trait]
impl Provider for ShapedChat {
    fn wire_shape(&self, request: &ModelRequest) -> Option<bubblepaw_core::WireShape> {
        let messages: Vec<Value> = request
            .messages
            .iter()
            .map(|message| json!({"role":message.role,"content":message.content}))
            .collect();
        let body =
            json!({"model":request.model,"system":request.system_prompt,"messages":messages});
        Some(bubblepaw_core::WireShape::from_body(&body, "messages", 64))
    }
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ModelResponse {
            text: "{\"choice\":\"2\"}".into(),
            usage: Some(Usage {
                input_tokens: 1000,
                output_tokens: 4,
                cache_read_tokens: if call == 0 { 0 } else { 900 },
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}
#[tokio::test]
async fn chat_decider_calls_are_cache_accounted_without_declaring_busts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trace.jsonl");
    let trace = JsonlTrace::open(TraceConfig {
        path: path.clone(),
        ..Default::default()
    })
    .unwrap();
    let mut registry = ProviderRegistry::new("main", Arc::new(NoProvider));
    registry
        .insert("opencode", Arc::new(ShapedChat::default()))
        .unwrap();
    let mut config = EpisodeModelConfig::default();
    config.roles.decider.models = vec![
        "openai/gpt-6-luna".into(),
        "llm/opencode/deepseek-v4.1-flash".into(),
    ];
    let models =
        HttpEpisodeModels::from_registry(config, Arc::new(registry), Some(trace.clone())).unwrap();
    assert_eq!(
        models.decide(split()).await.unwrap().choice.as_deref(),
        Some("u2")
    );
    trace.flush(Duration::from_secs(2)).unwrap();
    let text = std::fs::read_to_string(path).unwrap();
    let calls: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter_map(|record| {
            let record = record["record"]["metrics"].clone();
            (record["event"] == "episode_model_call").then_some(record)
        })
        .collect();
    assert_eq!(calls.len(), 3, "{text}");
    for call in &calls {
        assert_eq!(call["kind"], "decider_chat");
        assert_eq!(
            call["cache_group"],
            json!({"provider":"opencode","model":"deepseek-v4.1-flash","kind":"decider"})
        );
        assert_eq!(call["prefix_break"], Value::Null);
    }
    // The first sample has no prior; identical repeats expect the whole prefix,
    // rounded down to the 64-token cache block.
    assert_eq!(calls[0]["expected_cacheable_tokens"], 0);
    for call in &calls[1..] {
        assert_eq!(call["expected_cacheable_tokens"], 960);
        assert_eq!(call["measured_cache_read_tokens"], 900);
        assert_eq!(call["unexplained_miss_tokens"], 60);
    }
    assert!(!text.contains("cache_bust"));
}

#[tokio::test]
async fn unconfigured_decider_fails_at_first_split_question_and_names_its_key() {
    use bubblepaw_runner::model_roles::ModelRoles;
    let registry = ProviderRegistry::new("main", Arc::new(NoProvider));
    let models = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles: ModelRoles::default(),
            retry_base_delay: Duration::ZERO,
        },
        Arc::new(registry),
        None,
    )
    .unwrap();
    // A run with no boundary to choose between needs no decider.
    let mut no_options = split();
    no_options.options.clear();
    assert!(models.decide(no_options).await.unwrap().choice.is_none());
    let error = models.decide(split()).await.unwrap_err();
    assert!(error.to_string().contains("roles.decider"), "{error}");
}
