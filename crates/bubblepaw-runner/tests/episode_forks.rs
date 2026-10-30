use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextPreparation, Harness, HarnessConfig, Message, MessageOrigin,
    ModelRequest, ModelResponse, Provider, ReasoningEffort, Role, RunOptions, Store, TextCallback,
    ToolCall, ToolDefinition, ToolRegistry,
};
use bubblepaw_runner::{
    episode_models::{
        ChapterInput, ChapterResult, EpisodeFork, EpisodeModels, SplitInput, SplitResult,
        SummaryInput, SummaryResult,
    },
    episode_worker::EpisodeWorker,
    summarization::{SummarizingContext, SummaryConfig},
    usage::UsageContext,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
#[path = "common/hook.rs"]
mod hook;
use hook::prepare_fresh;

#[derive(Default)]
struct Recorded(Mutex<Vec<ModelRequest>>);
#[async_trait]
impl Provider for Recorded {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.0.lock().unwrap().push(request);
        Ok(ModelResponse {
            text: "done".into(),
            ..Default::default()
        })
    }
}
struct Fixture {
    _directory: tempfile::TempDir,
    store: Store,
    session: String,
    provider: Arc<Recorded>,
    context: Arc<SummarizingContext>,
}
impl Fixture {
    fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let store = Store::open(directory.path().join("fork.sqlite"))?;
        let session = store.create_session("fork")?.id;
        let provider = Arc::new(Recorded::default());
        let context = Arc::new(
            SummarizingContext::new(
                store.clone(),
                provider.clone(),
                SummaryConfig::default(),
                None,
            )?
            .with_auto_summarize(false),
        );
        Ok(Self {
            _directory: directory,
            store,
            session,
            provider,
            context,
        })
    }
    fn message(&self, role: Role, content: impl Into<String>) -> Message {
        Message {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: self.session.clone(),
            role: role.clone(),
            origin: Some(if role == Role::User {
                MessageOrigin::Typed
            } else {
                MessageOrigin::Generated
            }),
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: "2026-10-18T00:00:00Z".into(),
        }
    }
    fn request(&self, messages: Vec<Message>) -> ModelRequest {
        let tool = |name: &str| ToolDefinition {
            name: name.into(),
            description: "unchanged declaration".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Default::default(),
        };
        ModelRequest {
            model: "route/exact-model".into(),
            system_prompt: "ORIGINAL\nsystem prompt".into(),
            messages,
            tools: vec![tool("tool")],
            disabled_tools: vec![tool("inactive")],
            max_output_tokens: Some(8192),
            reasoning_effort: Some(ReasoningEffort::High),
            session_id: self.session.clone(),
            ..Default::default()
        }
    }
    fn capture(&self, request: &ModelRequest) {
        self.context
            .observe_request(&self.session, request, self.provider.clone());
        self.context
            .observe_response(&self.session, &ModelResponse::default());
    }
}

#[test]
fn fork_preserves_complete_prefix_and_main_settings_and_removes_future() -> Result<()> {
    let f = Fixture::new()?;
    let messages = vec![
        f.message(Role::User, "earlier context"),
        f.message(Role::Assistant, "FIRST"),
        f.message(Role::Assistant, "END"),
        f.message(Role::User, "FUTURE SECRET"),
    ];
    f.store.append_batch(&messages)?;
    let mut expected = f.request(messages.clone());
    f.capture(&expected);
    let fork = f
        .context
        .episode_fork(&f.session, &messages[1].id, &messages[2].id)?
        .unwrap();
    expected.messages.truncate(3);
    assert_eq!(
        serde_json::to_value(fork.request)?,
        serde_json::to_value(expected)?
    );
    assert!(fork.dispatched);
    assert!(Arc::ptr_eq(
        &fork.provider,
        &(f.provider.clone() as Arc<dyn Provider>)
    ));
    Ok(())
}

#[test]
fn old_capture_appends_exact_suffix_with_full_middle_tool_evidence() -> Result<()> {
    let f = Fixture::new()?;
    let user = f.message(Role::User, "begin");
    let mut call = f.message(Role::Assistant, "tool");
    call.tool_calls = vec![ToolCall {
        id: "call".into(),
        name: "tool".into(),
        arguments: json!({"large": "argument".repeat(6000)}),
    }];
    let mut output = f.message(
        Role::Tool,
        format!(
            "{}UNIQUE MIDDLE EVIDENCE{}",
            "before ".repeat(5000),
            "after ".repeat(5000)
        ),
    );
    output.tool_call_id = Some("call".into());
    output.provider_state = Some(json!({"opaque":"preserve exact provider state"}));
    let source = vec![
        user,
        call,
        output,
        f.message(Role::Assistant, "end"),
        f.message(Role::User, "FUTURE SECRET"),
    ];
    f.store.append_batch(&source)?;
    f.capture(&f.request(source[..1].to_vec()));
    let fork = f
        .context
        .episode_fork(&f.session, &source[1].id, &source[3].id)?
        .unwrap();
    assert_eq!(fork.request.messages, source[..4]);
    assert!(fork.request.messages[2].content.len() > 4000);
    assert!(
        fork.request.messages[2]
            .content
            .contains("UNIQUE MIDDLE EVIDENCE")
    );
    assert_eq!(f.store.messages(&f.session)?, source);
    Ok(())
}

#[test]
fn projected_or_missing_raw_and_incomplete_tool_groups_fall_back() -> Result<()> {
    let f = Fixture::new()?;
    let mut call = f.message(Role::Assistant, "call");
    call.tool_calls = vec![ToolCall {
        id: "call".into(),
        name: "tool".into(),
        arguments: json!({}),
    }];
    let mut output = f.message(Role::Tool, "raw exact output");
    output.tool_call_id = Some("call".into());
    f.store.append_batch(&[call.clone(), output.clone()])?;
    assert!(
        f.context
            .episode_fork(&f.session, &call.id, &output.id)?
            .is_none()
    );
    f.capture(&f.request(vec![call.clone(), output.clone()]));
    assert!(
        f.context
            .episode_fork(&f.session, &call.id, &call.id)?
            .is_none()
    );
    let mut projected = output.clone();
    projected.content = "[Tool output masked]".into();
    f.capture(&f.request(vec![call.clone(), projected]));
    assert!(
        f.context
            .episode_fork(&f.session, &call.id, &output.id)?
            .is_none()
    );
    f.capture(&f.request(vec![output.clone()]));
    assert!(
        f.context
            .episode_fork(&f.session, &call.id, &output.id)?
            .is_none()
    );
    Ok(())
}

#[test]
fn later_branch_control_and_future_task_anchor_cannot_leak() -> Result<()> {
    let f = Fixture::new()?;
    let source = vec![
        f.message(Role::User, "first"),
        f.message(Role::Assistant, "end"),
        f.message(Role::User, "FUTURE SECRET"),
    ];
    f.store.append_batch(&source)?;
    let mut request = f.request(source[..2].to_vec());
    request
        .messages
        .push(f.message(Role::System, "future schema correction"));
    f.capture(&request);
    assert_eq!(
        f.context
            .episode_fork(&f.session, &source[0].id, &source[1].id)?
            .unwrap()
            .request
            .messages,
        source[..2]
    );
    f.capture(&f.request(vec![
        source[2].clone(),
        source[0].clone(),
        source[1].clone(),
    ]));
    assert!(
        f.context
            .episode_fork(&f.session, &source[0].id, &source[1].id)?
            .is_none()
    );
    Ok(())
}

#[test]
fn fallback_response_updates_route_without_mutating_already_built_fork() -> Result<()> {
    let f = Fixture::new()?;
    let source = f.message(Role::User, "source");
    f.store.append_batch(std::slice::from_ref(&source))?;
    f.capture(&f.request(vec![source.clone()]));
    let old = f
        .context
        .episode_fork(&f.session, &source.id, &source.id)?
        .unwrap();
    f.context.observe_response(
        &f.session,
        &ModelResponse {
            provider_state: Some(json!({"_bubblepaw_selected_model":"paid/actual-success"})),
            ..Default::default()
        },
    );
    let new = f
        .context
        .episode_fork(&f.session, &source.id, &source.id)?
        .unwrap();
    assert_eq!(old.request.model, "route/exact-model");
    assert_eq!(new.request.model, "paid/actual-success");
    Ok(())
}

#[tokio::test]
async fn actual_main_dispatch_through_usage_wrapper_captures_child_and_releases() -> Result<()> {
    let f = Fixture::new()?;
    let child = f.store.fork(&f.session, "native child")?;
    let harness = Harness::new(
        f.store.clone(),
        f.provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            context: Arc::new(UsageContext::new(f.store.clone(), f.context.clone())),
            ..Default::default()
        },
    );
    harness
        .prompt(
            &child.id,
            "child task",
            RunOptions {
                system_prompt: Some("exact child instructions".into()),
                max_output_tokens: Some(200),
                ..Default::default()
            },
        )
        .await?;
    let main = f.provider.0.lock().unwrap()[0].clone();
    let first = main.messages.first().unwrap().id.clone();
    let end = main.messages.last().unwrap().id.clone();
    let fork = f.context.episode_fork(&child.id, &first, &end)?.unwrap();
    assert_eq!(
        serde_json::to_value(fork.request)?,
        serde_json::to_value(main)?
    );
    assert!(fork.dispatched);
    assert!(f.context.episode_fork(&f.session, &first, &end)?.is_none());
    harness.release_idle_resources(&child.id).await?;
    assert!(f.context.episode_fork(&child.id, &first, &end)?.is_none());
    Ok(())
}

#[path = "episode_forks/sticky.rs"]
mod sticky;

#[derive(Default)]
struct ForkModels {
    forks: Mutex<Vec<EpisodeFork>>,
    bounded: Mutex<usize>,
}
#[async_trait]
impl EpisodeModels for ForkModels {
    async fn summarize(&self, _: SummaryInput) -> Result<SummaryResult> {
        *self.bounded.lock().unwrap() += 1;
        Ok(SummaryResult {
            title: "episode".into(),
            summary: "memory".into(),
            model: "bounded".into(),
            usage: Default::default(),
            reasoning: None,
        })
    }
    async fn summarize_fork(&self, _: SummaryInput, fork: EpisodeFork) -> Result<SummaryResult> {
        let model = fork.request.model.clone();
        self.forks.lock().unwrap().push(fork);
        Ok(SummaryResult {
            title: "episode".into(),
            summary: "memory".into(),
            model,
            usage: Default::default(),
            reasoning: None,
        })
    }
    async fn decide(&self, input: SplitInput) -> Result<SplitResult> {
        let mut probabilities = vec![0.0; input.options.len() + 1];
        *probabilities.last_mut().unwrap() = 1.0;
        Ok(SplitResult {
            choice: None,
            probabilities,
            model: "decision".into(),
            usage: Default::default(),
            reasoning: None,
        })
    }
    async fn close_chapter(&self, _: ChapterInput) -> Result<ChapterResult> {
        anyhow::bail!("short test must not close a chapter")
    }
}
#[tokio::test]
async fn episode_worker_uses_fork_and_cleanup_releases_captures() -> Result<()> {
    let f = Fixture::new()?;
    let source = vec![
        f.message(Role::User, "task"),
        f.message(Role::Assistant, "complete exact source"),
    ];
    f.store.append_batch(&source)?;
    f.capture(&f.request(source.clone()));
    let models = Arc::new(ForkModels::default());
    let worker = EpisodeWorker::new(f.store.clone(), models.clone(), f.context.clone());
    worker.compact_until(&f.session, source.len(), None).await?;
    assert_eq!(*models.bounded.lock().unwrap(), 0);
    let forks = models.forks.lock().unwrap();
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0].request.messages, source);
    assert_eq!(forks[0].first_message_id, source[0].id);
    assert_eq!(forks[0].last_message_id, source[1].id);
    assert_eq!(
        f.store.context_episodes(&f.session)?[0].model,
        "route/exact-model"
    );
    drop(forks);
    worker.cancel_session(&f.session);
    assert!(
        f.context
            .episode_fork(&f.session, &source[0].id, &source[1].id)?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn pressure_catch_up_uses_explicit_cold_candidate_before_first_dispatch() -> Result<()> {
    let mut f = Fixture::new()?;
    let mut config = SummaryConfig {
        threshold_cap: 16_000,
        verbatim_user_tokens: 64,
        ..Default::default()
    };
    config.lcm.raw_tail_tokens = 32;
    config.lcm.output_reserve_tokens = 0;
    config.lcm.episodes.source_target_tokens = 64;
    config.lcm.episodes.source_max_tokens = 512;
    f.context = Arc::new(SummarizingContext::new(
        f.store.clone(),
        f.provider.clone(),
        config.clone(),
        None,
    )?);
    let source: Vec<_> = (0..10)
        .flat_map(|i| {
            [
                f.message(Role::User, format!("task {i}")),
                f.message(Role::Assistant, " exact evidence".repeat(2_000)),
            ]
        })
        .collect();
    f.store.append_batch(&source)?;
    let models = Arc::new(ForkModels::default());
    let worker = EpisodeWorker::new_with_config(
        f.store.clone(),
        models.clone(),
        f.context.clone(),
        config.lcm.episodes,
    )?;
    f.context.set_episode_worker(&worker);
    let request = f.request(source);
    assert!(f.context.estimated_tokens(&request) > config.threshold_cap);
    let prepared = prepare_fresh(&*f.context, f.provider.clone(), &f.session, request).await?;
    assert!(prepared.notice.is_some());
    assert!(
        f.store
            .context_view(&f.session)?
            .unwrap()
            .frontier
            .is_some()
    );
    assert!(f.context.estimated_tokens(&prepared.request) < config.threshold_cap);
    let forks = models.forks.lock().unwrap();
    assert!(!forks.is_empty());
    assert!(forks.iter().all(|fork| !fork.dispatched));
    assert_eq!(*models.bounded.lock().unwrap(), 0);
    assert!(f.provider.0.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn idle_child_release_gathers_final_fork_and_preserves_resumed_capture() -> Result<()> {
    let mut f = Fixture::new()?;
    let mut config = SummaryConfig::default();
    config.lcm.episodes.source_target_tokens = 1;
    f.context = Arc::new(SummarizingContext::new(
        f.store.clone(),
        f.provider.clone(),
        config.clone(),
        None,
    )?);
    let source = vec![
        f.message(Role::User, "task"),
        f.message(Role::Assistant, "final exact source"),
    ];
    f.store.append_batch(&source)?;
    let request = f.request(source.clone());
    f.capture(&request);
    let models = Arc::new(ForkModels::default());
    let worker = EpisodeWorker::new_with_config(
        f.store.clone(),
        models.clone(),
        f.context.clone(),
        config.lcm.episodes,
    )?;
    f.context.set_episode_worker(&worker);
    f.context.release_session(&f.session);
    worker.wait_idle(&f.session).await;
    assert_eq!(models.forks.lock().unwrap().len(), 1);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while f
            .context
            .episode_fork(&f.session, &source[0].id, &source[1].id)
            .unwrap()
            .is_some()
        {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    f.capture(&request);
    f.context.release_session(&f.session);
    // A new observation denotes resumed work; old deferred cleanup must not
    // discard that new session-owned snapshot when its drain finishes.
    f.capture(&request);
    worker.wait_idle(&f.session).await;
    tokio::task::yield_now().await;
    assert!(
        f.context
            .episode_fork(&f.session, &source[0].id, &source[1].id)?
            .is_some()
    );
    worker.shutdown_wait().await;
    assert!(
        f.context
            .episode_fork(&f.session, &source[0].id, &source[1].id)?
            .is_none()
    );
    Ok(())
}
