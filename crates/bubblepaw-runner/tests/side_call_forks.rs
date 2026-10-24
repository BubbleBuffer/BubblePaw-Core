//! Same-model side calls fork the chat's context: their request starts with the
//! parent's last request, and fresh side calls declare why they are not forks.
#[path = "support/chat_messages.rs"]
mod chat_messages;
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CacheTiers, CancellationToken, MessageOrigin, ModelRequest, ModelResponse, Provider,
    ReasoningEffort, Role, Store, TextCallback, ToolCall, ToolDefinition, Usage,
};
use bubblepaw_runner::{
    episode_models::{
        ChapterInput, EpisodeFork, EpisodeModelConfig, EpisodeModels, HttpEpisodeModels,
        SplitInput, SplitOption, SummaryInput,
    },
    model_roles::ModelRoles,
    provider::ProviderRegistry,
    trace::{JsonlTrace, TraceConfig},
};
use chat_messages::root_message;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

/// Records every request; answers the first `tool_calls` requests with a tool call.
#[derive(Default)]
struct Capture {
    requests: Mutex<Vec<ModelRequest>>,
    tool_calls: Mutex<usize>,
}
#[async_trait]
impl Provider for Capture {
    fn supports_summary_fork(&self, _: &str) -> bool {
        true
    }
    fn summary_fork_mapping(&self, request: &ModelRequest) -> Result<Option<Value>> {
        Ok(Some(json!({"sources":request.messages.iter()
            .map(|message| json!({"source_id":message.id})).collect::<Vec<_>>()})))
    }
    fn estimate_summary_fork_tokens(
        &self,
        _: &ModelRequest,
        _: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        Ok(Some(10))
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
        let decision = request.system_prompt.contains("Allowed options:");
        self.requests.lock().unwrap().push(request);
        let usage = Some(Usage {
            input_tokens: 12,
            output_tokens: 4,
            ..Default::default()
        });
        let mut left = self.tool_calls.lock().unwrap();
        if *left > 0 {
            *left -= 1;
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: format!("call-{left}"),
                    name: "read".into(),
                    arguments: json!({"path":"x"}),
                }],
                usage,
                ..Default::default()
            });
        }
        let text = if decision {
            r#"{"choice":"none"}"#
        } else {
            "# Recorded memory\nThe source evidence remains unchanged.\nRelated: none"
        };
        Ok(ModelResponse {
            text: text.into(),
            usage,
            ..Default::default()
        })
    }
}

struct Fixture {
    provider: Arc<Capture>,
    models: HttpEpisodeModels,
    trace: JsonlTrace,
    store: Store,
    session: String,
    path: PathBuf,
    _temp: tempfile::TempDir,
}
fn fixture() -> Result<Fixture> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("trace.jsonl");
    let trace = JsonlTrace::open(TraceConfig {
        path: path.clone(),
        ..Default::default()
    })?;
    let provider = Arc::new(Capture::default());
    let store = Store::in_memory()?;
    let session = store.create_session("Side calls")?.id;
    let registry = Arc::new(ProviderRegistry::new("script", provider.clone()));
    registry.set_usage_store(store.clone());
    let mut roles = ModelRoles::for_models("script/model".into(), String::new());
    roles.decider.models = vec!["llm/script/model".into()];
    roles.decider.repeats = Some(1);
    let models = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles,
            retry_base_delay: Duration::ZERO,
        },
        registry,
        Some(trace.clone()),
    )?;
    Ok(Fixture {
        provider,
        models,
        trace,
        store,
        session,
        path,
        _temp: temp,
    })
}
impl Fixture {
    /// Send `parent` as the main lineage's last request, as the main loop does.
    fn parent(&self, parent: &ModelRequest) {
        let mut earlier = parent.clone();
        earlier.messages.pop();
        let ledger = self.store.cache_accounting();
        let context = &ledger.context;
        context.build(earlier, &self.session, "root", &[0]);
        context.build(parent.clone(), &self.session, "root", &[0]);
    }
    fn fork(&self, request: &ModelRequest, last: &str) -> EpisodeFork {
        self.models
            .record_fork_context(&self.session, request, &[0]);
        EpisodeFork {
            request: request.clone(),
            first_message_id: "m2".into(),
            last_message_id: last.into(),
            provider: self.provider.clone(),
            max_request_bytes: 1_000_000,
            context_window: Some(1_000_000),
            output_reserve_tokens: 100,
            dispatched: true,
        }
    }
    fn built(&self) -> Result<Vec<Value>> {
        self.trace.flush(Duration::from_secs(3))?;
        Ok(std::fs::read_to_string(&self.path)?
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).map(|row| row["record"].clone()))
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|row| row["kind"] == "context_built")
            .collect())
    }
    fn requests(&self) -> Vec<ModelRequest> {
        self.provider.requests.lock().unwrap().clone()
    }
}

/// The main loop's last request: cold notes, then two source rows.
fn parent() -> ModelRequest {
    ModelRequest {
        model: "script/model".into(),
        system_prompt: "main system".into(),
        tools: vec![ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Default::default(),
        }],
        reasoning_effort: Some(ReasoningEffort::Low),
        messages: vec![
            root_message("cold", "applied memory"),
            root_message("m2", "selected evidence"),
            root_message("m3", "latest evidence"),
        ],
        ..Default::default()
    }
}
fn summary(session: &str) -> SummaryInput {
    SummaryInput {
        session_id: session.into(),
        focus: None,
        previous_summary: None,
        turn_request: None,
        episode_text: "Canonical evidence".into(),
        cap: 2000,
    }
}
fn chapter(session: &str) -> ChapterInput {
    ChapterInput {
        session_id: session.into(),
        episodes: vec!["EPISODE 1 memory".into()],
        previous_chapters: vec![],
        typed_user_texts: vec!["fix the parser".into()],
        task_instructions: vec![],
    }
}

/// The parent's bytes: system prompt, tools and the first `len` messages.
fn prefix(request: &ModelRequest, len: usize) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&json!({
        "system": request.system_prompt,
        "tools": request.tools,
        "messages": &request.messages[..len],
    }))?)
}
const PARENT_TIERS: CacheTiers = CacheTiers {
    cold_end: Some(1),
    previous_len: Some(3),
};

#[tokio::test]
async fn summary_fork_sends_the_parent_prefix_with_its_cache_tiers() -> Result<()> {
    let f = fixture()?;
    let mut parent = parent();
    parent.output_schema = Some(bubblepaw_core::JsonSchema::new(
        "legacy",
        json!({"type":"object","additionalProperties":false}),
    )?);
    f.parent(&parent);
    f.models
        .summarize_fork(summary(&f.session), f.fork(&parent, "m3"))
        .await?;
    let sent = f.requests();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].output_schema.is_none());
    assert_eq!(prefix(&sent[0], 3)?, prefix(&parent, 3)?);
    // The measured summary control follows the prefix unchanged.
    assert_eq!(sent[0].messages.len(), 5);
    assert_eq!(sent[0].messages[3].role, Role::System);
    assert!(
        sent[0].messages[4]
            .content
            .contains("SUMMARY RETURN CONTROL")
    );
    // The fork reads the block the parent's last request wrote.
    assert_eq!(sent[0].cache_tiers, Some(PARENT_TIERS));
    let built = f.built()?;
    let fork = built.last().unwrap();
    assert_eq!(fork["metrics"]["request_kind"], "summary_fork");
    assert_eq!(fork["metrics"]["classification"]["kind"], "append");
    assert_eq!(fork["metrics"]["declared"], false);
    Ok(())
}

#[tokio::test]
async fn chapter_fork_sends_the_parent_prefix_then_the_chapter_task() -> Result<()> {
    let f = fixture()?;
    let mut parent = parent();
    parent.output_schema = Some(bubblepaw_core::JsonSchema::new(
        "legacy",
        json!({"type":"object","additionalProperties":false}),
    )?);
    f.parent(&parent);
    let result = f
        .models
        .close_chapter_fork(chapter(&f.session), f.fork(&parent, "m3"))
        .await?;
    assert_eq!(result.description, "Recorded memory");
    let sent = f.requests();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].output_schema.is_none());
    assert_eq!(prefix(&sent[0], 3)?, prefix(&parent, 3)?);
    assert_eq!(sent[0].reasoning_effort, parent.reasoning_effort);
    let task = &sent[0].messages[3..];
    assert_eq!(task.len(), 1);
    assert_eq!(task[0].role, Role::User);
    assert_eq!(task[0].origin, Some(MessageOrigin::Generated));
    assert!(
        task[0]
            .content
            .starts_with("<harness kind=\"chapter_task\">")
    );
    assert!(task[0].content.contains("TYPED REQUEST:\nfix the parser"));
    assert_eq!(sent[0].cache_tiers, Some(PARENT_TIERS));
    let built = f.built()?;
    let fork = built.last().unwrap();
    assert_eq!(fork["metrics"]["request_kind"], "chapter_fork");
    assert_eq!(fork["metrics"]["classification"]["kind"], "append");
    assert_eq!(fork["metrics"]["declared"], false);
    Ok(())
}

#[tokio::test]
async fn fresh_side_calls_declare_why_they_are_not_forks() -> Result<()> {
    let f = fixture()?;
    f.models.close_chapter(chapter(&f.session)).await?;
    // The chat runs another reasoning effort than the chapter role.
    let mut other = parent();
    other.reasoning_effort = None;
    f.parent(&other);
    f.models
        .close_chapter_fork(chapter(&f.session), f.fork(&other, "m3"))
        .await?;
    f.models.summarize(summary(&f.session)).await?;
    f.models
        .decide(SplitInput {
            session_id: f.session.clone(),
            open_chapter: "state".into(),
            options: vec![SplitOption {
                message_id: "m1".into(),
                user: "task".into(),
                agent_before: "progress".into(),
            }],
        })
        .await?;
    let sent = f.requests();
    assert_eq!(sent.len(), 4);
    assert!(sent.iter().all(|request| request.cache_tiers.is_none()));
    assert_eq!(sent[1].messages.len(), 1, "a fresh chapter has no prefix");
    let built = f.built()?;
    let reasons: Vec<_> = built
        .iter()
        .map(|row| {
            assert_eq!(row["metrics"]["declared"], true, "{row}");
            (
                row["metrics"]["request_kind"].as_str().unwrap(),
                row["metrics"]["declaration_reason"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(reasons[0], ("chapter", "side_call_no_capture"));
    assert_eq!(reasons[1], ("chapter", "side_call_settings"));
    assert_eq!(reasons[2], ("summary", "side_call_episode_input"));
    assert!(reasons[3].0.starts_with("decider"));
    assert!(reasons[3].1.starts_with("side_call_"));
    Ok(())
}

#[tokio::test]
async fn a_tool_call_is_refused_once_then_the_fork_is_retried() -> Result<()> {
    let f = fixture()?;
    let parent = parent();
    f.parent(&parent);
    *f.provider.tool_calls.lock().unwrap() = 1;
    let result = f
        .models
        .close_chapter_fork(chapter(&f.session), f.fork(&parent, "m3"))
        .await?;
    assert_eq!(result.usage.input_tokens, 24, "both attempts are billed");
    let sent = f.requests();
    assert_eq!(sent.len(), 2);
    let (first, retry) = (&sent[0], &sent[1]);
    assert_eq!(retry.messages[..4], first.messages[..4]);
    assert_eq!(retry.tools.len(), parent.tools.len());
    let refusal = &retry.messages[4..];
    assert_eq!(refusal.len(), 2);
    assert_eq!(refusal[0].role, Role::Assistant);
    assert_eq!(refusal[0].tool_calls[0].id, "call-0");
    assert_eq!(refusal[1].role, Role::Tool);
    assert_eq!(refusal[1].tool_call_id.as_deref(), Some("call-0"));
    assert!(refusal[1].is_error);
    assert_eq!(refusal[1].content, "No tools in this task; answer in text.");
    assert_eq!(retry.cache_tiers, Some(PARENT_TIERS));

    // A second tool call fails as before, after one retry.
    let f = fixture()?;
    f.parent(&parent);
    *f.provider.tool_calls.lock().unwrap() = 2;
    let error = f
        .models
        .summarize_fork(summary(&f.session), f.fork(&parent, "m3"))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("tool_call_returned"),
        "{error:#}"
    );
    assert_eq!(f.requests().len(), 2);
    Ok(())
}
