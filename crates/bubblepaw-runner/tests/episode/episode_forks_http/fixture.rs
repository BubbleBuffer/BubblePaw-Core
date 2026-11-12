//! Shared local Chat HTTP fixture and the exact captured parent request.
use super::*;

#[derive(Default)]
pub(super) struct Wire {
    pub(super) requests: Mutex<Vec<(HeaderMap, Value)>>,
    replies: Mutex<VecDeque<Value>>,
}
async fn receive(
    State(wire): State<Arc<Wire>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    wire.requests.lock().unwrap().push((headers, body));
    Json(
        wire.replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected network call"),
    )
}
pub(super) struct Fixture {
    pub(super) wire: Arc<Wire>,
    pub(super) registry: Arc<ProviderRegistry>,
    pub(super) server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
pub(super) async fn fixture(replies: Vec<Value>) -> Result<Fixture> {
    let wire = Arc::new(Wire {
        replies: Mutex::new(replies.into()),
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let app = Router::new()
        .route("/chat/completions", post(receive))
        .with_state(wire.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider = ChatCompletionsProvider::without_key(base)?
        .with_opencode_route()
        .with_reasoning_replay(BTreeMap::from([("flash".into(), ReasoningReplay::All)]))?;
    let registry = Arc::new(ProviderRegistry::new("opencode", Arc::new(provider)));
    Ok(Fixture {
        wire,
        registry,
        server,
    })
}
pub(super) fn reply(text: &str) -> Value {
    json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":text}}],"usage":{"prompt_tokens":501,"completion_tokens":23,"prompt_tokens_details":{"cached_tokens":400}}})
}
pub(super) fn message(id: &str, role: Role, content: &str) -> Message {
    Message {
        id: id.into(),
        session_id: "source-session".into(),
        role,
        origin: None,
        content: content.into(),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: String::new(),
    }
}
pub(super) fn request() -> ModelRequest {
    let mut assistant = message("source-a", Role::Assistant, "Checking the result.");
    assistant.tool_calls.push(ToolCall {
        id: "call-exact".into(),
        name: "js".into(),
        arguments: json!({"code":"return await tools.check()"}),
    });
    assistant.provider_state = Some(bubblepaw_core::with_provider_state_origin(json!({
        "adapter":"chat-completions", "rounds":[
            {"text_range":[0,9],"message_fields":{"reasoning_content":"First native thought"}},
            {"text_range":[9,20],"message_fields":{"reasoning_content":"Second native thought"}},
        ]
    }), "chat-completions", Some("flash"), "native").unwrap());
    let mut tool = message(
        "source-t",
        Role::Tool,
        &format!(
            "head\n{}\nMIDDLE_EXACT_FAILURE exit=7; corrected path src/fix.rs\n{}\ntail",
            "x".repeat(10_000),
            "y".repeat(10_000)
        ),
    );
    tool.tool_call_id = Some("call-exact".into());
    ModelRequest {
        model: "opencode/flash".into(),
        system_prompt: "Frozen original opening instructions".into(),
        messages: vec![
            message("source-u", Role::User, "Fix the recorded failure"),
            assistant,
            tool,
        ],
        tools: vec![ToolDefinition {
            name: "js".into(),
            description: "Frozen js declaration".into(),
            input_schema: json!({"type":"object","properties":{"code":{"type":"string"}}}),
            read_only: false,
            approval: Default::default(),
        }],
        disabled_tools: vec![ToolDefinition {
            name: "inactive".into(),
            description: "Retained inactive declaration".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Default::default(),
        }],
        max_output_tokens: Some(8000),
        reasoning_effort: Some(ReasoningEffort::High),
        session_id: "source-session".into(),
        ..Default::default()
    }
}
pub(super) fn input() -> SummaryInput {
    SummaryInput {
        focus: None,
        session_id: "source-session".into(),
        previous_summary: None,
        turn_request: Some("Fix the recorded failure".into()),
        episode_text: "BOUNDED_INPUT_SENTINEL".into(),
        cap: 2000,
    }
}
pub(super) fn fork(registry: Arc<ProviderRegistry>, request: ModelRequest) -> EpisodeFork {
    EpisodeFork {
        request,
        first_message_id: "source-u".into(),
        last_message_id: "source-t".into(),
        provider: registry,
        max_request_bytes: 2 * 1024 * 1024,
        context_window: Some(200_000),
        output_reserve_tokens: 4000,
        dispatched: true,
    }
}
pub(super) fn models(
    registry: Arc<ProviderRegistry>,
    trace: Option<JsonlTrace>,
) -> Result<HttpEpisodeModels> {
    let mut roles = ModelRoles::for_models("opencode/flash".into(), "opencode/flash".into());
    // Decisions name their chain explicitly; Luna is unregistered here, so the LLM route serves.
    roles.decider.models = vec!["openai/gpt-6-luna".into(), "llm/opencode/flash".into()];
    HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles,
            retry_base_delay: Duration::ZERO,
        },
        registry,
        trace,
    )
}
