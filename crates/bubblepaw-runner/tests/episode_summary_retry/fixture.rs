//! Deterministic side-call and fork provider.
use super::*;

pub(super) const SUMMARY: &str =
    "# Recorded repair\nThe parser fix passed its focused check [source-a].";
pub(super) const RETRY: &str = "HOST-GENERATED SUMMARY RETRY CONTROL";

pub(super) struct Script {
    replies: Mutex<VecDeque<ModelResponse>>,
    pub(super) requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Provider for Script {
    fn supports_summary_fork(&self, _: &str) -> bool {
        true
    }
    fn summary_fork_mapping(&self, request: &ModelRequest) -> Result<Option<Value>> {
        Ok(Some(
            json!({"sources": request.messages.iter().map(|message| {
            json!({"source_id": message.id})
        }).collect::<Vec<_>>()}),
        ))
    }
    fn estimate_summary_fork_tokens(
        &self,
        request: &ModelRequest,
        _: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        // A deterministic synthetic physical size makes the retry's admission
        // observable without depending on a tokenizer implementation.
        Ok(Some(
            if request
                .messages
                .iter()
                .any(|m| m.content.starts_with(RETRY))
            {
                101
            } else {
                100
            },
        ))
    }
    async fn generate_summary_fork(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        self.generate(request, Arc::new(|_| {}), cancellation).await
    }
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unplanned model call"))
    }
}

pub(super) fn response(text: &str) -> ModelResponse {
    ModelResponse {
        text: text.into(),
        provider_state: Some(json!({"reasoning_content":"synthetic reasoning without an answer"})),
        usage: Some(Usage {
            input_tokens: 17,
            output_tokens: 7,
            cache_read_tokens: 5,
            cache_creation_tokens: 2,
            reasoning_tokens: Some(4),
        }),
        ..Default::default()
    }
}
pub(super) fn fixture(replies: Vec<ModelResponse>) -> Result<(Arc<Script>, HttpEpisodeModels)> {
    let provider = Arc::new(Script {
        replies: Mutex::new(replies.into()),
        requests: Mutex::new(Vec::new()),
    });
    let registry = Arc::new(ProviderRegistry::new("script", provider.clone()));
    let models = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles: ModelRoles::for_models("script/model".into(), "script/model".into()),
            retry_base_delay: Duration::ZERO,
        },
        registry,
        None,
    )?;
    Ok((provider, models))
}
pub(super) fn summary() -> SummaryInput {
    SummaryInput {
        focus: None,
        session_id: "session".into(),
        previous_summary: None,
        turn_request: Some("Fix the parser".into()),
        episode_text: "USER: Fix the parser\nASSISTANT: Done; the parser check passed.".into(),
        cap: 2000,
    }
}
pub(super) fn chapter() -> ChapterInput {
    ChapterInput {
        session_id: "session".into(),
        episodes: vec![SUMMARY.into()],
        previous_chapters: Vec::new(),
        typed_user_texts: vec!["Fix the parser".into()],
        task_instructions: Vec::new(),
    }
}
pub(super) fn fork(provider: Arc<Script>, native: bool) -> EpisodeFork {
    let message = |id: &str, role, content: &str| Message {
        id: id.into(),
        session_id: "session".into(),
        role,
        origin: None,
        content: content.into(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: String::new(),
    };
    let user = message("source-u", Role::User, "Fix the parser");
    let assistant = message("source-a", Role::Assistant, "Check passed.");
    let tools = vec![ToolDefinition {
        name: if native { "call_tool" } else { "js" }.into(),
        description: "Frozen main declaration".into(),
        input_schema: json!({"type":"object"}),
        read_only: false,
        approval: Default::default(),
    }];
    EpisodeFork {
        request: ModelRequest {
            model: "script/model".into(),
            system_prompt: "Frozen main instructions".into(),
            messages: vec![user, assistant],
            tools,
            reasoning_effort: Some(ReasoningEffort::Low),
            session_id: "session".into(),
            ..Default::default()
        },
        first_message_id: "source-u".into(),
        last_message_id: "source-a".into(),
        provider,
        max_request_bytes: 1024 * 1024,
        context_window: Some(200_000),
        output_reserve_tokens: 0,
        dispatched: true,
    }
}
pub(super) fn assert_usage(usage: &Usage, calls: u64) {
    assert_eq!(usage.input_tokens, 17 * calls);
    assert_eq!(usage.output_tokens, 7 * calls);
    assert_eq!(usage.cache_read_tokens, 5 * calls);
    assert_eq!(usage.cache_creation_tokens, 2 * calls);
    assert_eq!(usage.reasoning_tokens, Some(4 * calls));
}
pub(super) fn assert_nudge(first: &ModelRequest, retry: &ModelRequest) {
    assert_eq!(first.system_prompt, retry.system_prompt);
    assert_eq!(
        serde_json::to_value(&first.tools).unwrap(),
        serde_json::to_value(&retry.tools).unwrap()
    );
    assert_eq!(first.max_output_tokens, retry.max_output_tokens);
    assert_eq!(first.reasoning_effort, retry.reasoning_effort);
    assert!(retry.output_schema.is_none());
    assert_eq!(retry.messages.len(), first.messages.len() + 1);
    assert_eq!(&retry.messages[..first.messages.len()], &first.messages);
    let nudge = retry.messages.last().unwrap();
    assert_eq!(nudge.role, Role::User);
    assert_eq!(nudge.origin, Some(MessageOrigin::Generated));
    assert!(nudge.content.starts_with(RETRY));
    assert!(nudge.content.contains("Write the requested summary now"));
    assert!(
        nudge
            .content
            .contains("not a source event or human request")
    );
    assert!(nudge.content.len() < 200);
}
