use anyhow::{Result, bail};
#[path = "summarization/chapter_ids.rs"]
mod chapter_ids;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextChapter, ContextEpisode, ContextPreparation, ContextSubjectChapter,
    EpisodeBatch, Message, ModelRequest, ModelResponse, PreparedContext, Provider, Role, Store,
    SubjectChapterStatus, TextCallback, ToolCall, Usage,
};
use bubblepaw_runner::{
    episode_models::{
        ChapterInput, ChapterResult, EpisodeModels, SplitInput, SplitResult, SummaryInput,
        SummaryResult,
    },
    episode_worker::EpisodeWorker,
    summarization::{
        HISTORY_RETRIEVAL_INSTRUCTION, SummarizingContext, SummaryConfig, cached_context_window,
    },
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::Notify;
#[path = "common/hook.rs"]
mod hook;
use hook::{prepare_call, prepare_fresh};
fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn now() -> String {
    "2026-10-01T00:00:00Z".into()
}
struct Script;
impl Script {
    fn normal() -> Arc<Self> {
        Arc::new(Self)
    }
}
#[async_trait]
impl Provider for Script {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        bail!("main provider must not be called during compaction")
    }
}
#[derive(Default)]
struct Models {
    calls: AtomicUsize,
    fail: AtomicBool,
    fail_after_one: AtomicBool,
    pause: AtomicBool,
    pause_on_call: AtomicUsize,
    pressure_entered: Notify,
    entered: Notify,
    release: Notify,
    active: AtomicUsize,
    peak: AtomicUsize,
    summary_tokens: AtomicUsize,
}
#[async_trait]
impl EpisodeModels for Models {
    fn record_pressure_gather(&self, _: &str, _: bool) {
        self.pressure_entered.notify_one();
    }
    async fn decide(&self, input: SplitInput) -> Result<SplitResult> {
        let mut probabilities = vec![0.0; input.options.len() + 1];
        *probabilities.last_mut().unwrap() = 1.0;
        Ok(SplitResult {
            choice: None,
            probabilities,
            model: "fake".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
    async fn summarize(&self, _: SummaryInput) -> Result<SummaryResult> {
        let count = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        if self.pause.swap(false, Ordering::SeqCst)
            || self.pause_on_call.load(Ordering::SeqCst) == count
        {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst)
            || (self.fail_after_one.load(Ordering::SeqCst) && count > 1)
        {
            bail!("fake exhausted retries");
        }
        Ok(SummaryResult {
            title: format!("Episode {count}"),
            summary: format!(
                "Retained {count}.{}",
                " datum".repeat(self.summary_tokens.load(Ordering::SeqCst))
            ),
            model: "fake".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
    async fn close_chapter(&self, _: ChapterInput) -> Result<ChapterResult> {
        Ok(ChapterResult {
            description: "subject".into(),
            summary: "closed subject".into(),
            related_ids: vec![],
            related_evidence: vec![],
            decisions: None,
            sections: None,
            model: "fake".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
}
struct Fixture {
    _directory: tempfile::TempDir,
    path: std::path::PathBuf,
    store: Store,
    session: String,
}
impl Fixture {
    fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("archive.sqlite");
        let store = Store::open(&path)?;
        let session = store.create_session("view fixture")?.id;
        Ok(Self {
            _directory: directory,
            path,
            store,
            session,
        })
    }
    fn seed(&self, messages: &[Message]) -> Result<()> {
        self.store.append_batch(messages)
    }
    fn message(&self, role: Role, content: impl Into<String>) -> Message {
        Message {
            id: id(),
            session_id: self.session.clone(),
            role,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
            origin: None,
            provider_state: None,
            is_error: false,
            created_at: now(),
        }
    }
    fn turns(&self, count: usize, tokens: usize) -> Vec<Message> {
        (0..count)
            .flat_map(|i| {
                [
                    self.message(Role::User, format!("request {i}")),
                    self.message(Role::Assistant, " datum".repeat(tokens)),
                ]
            })
            .collect()
    }
    fn tool_rounds(&self, count: usize, tokens: usize) -> Vec<Message> {
        (0..count)
            .flat_map(|index| {
                let mut call = self.message(Role::Assistant, "");
                let call_id = format!("round-{index}");
                call.tool_calls.push(ToolCall {
                    id: call_id.clone(),
                    name: "js".into(),
                    arguments: json!({"code": "exact source ".repeat(400)}),
                });
                let mut result = self.message(Role::Tool, " datum".repeat(tokens));
                result.tool_call_id = Some(call_id);
                [call, result]
            })
            .collect()
    }
    fn request(&self, messages: Vec<Message>) -> ModelRequest {
        ModelRequest {
            model: "fake".into(),
            system_prompt: "Instructions".into(),
            messages,
            tools: vec![],
            max_output_tokens: None,
            ..Default::default()
        }
    }
    fn policy(&self, provider: Arc<dyn Provider>, cap: usize) -> Result<SummarizingContext> {
        SummarizingContext::new(
            self.store.clone(),
            provider,
            SummaryConfig {
                threshold_cap: cap,
                lcm: bubblepaw_runner::summarization::LcmConfig {
                    // Synthetic caps here measure only the prepared input.
                    output_reserve_tokens: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
            None,
        )
    }
    fn worker(
        &self,
        cap: usize,
    ) -> Result<(Arc<SummarizingContext>, Arc<EpisodeWorker>, Arc<Models>)> {
        let policy = Arc::new(self.policy(Script::normal(), cap)?);
        let models = Arc::new(Models::default());
        let worker = new_episode_worker(self.store.clone(), models.clone(), policy.clone());
        policy.set_episode_worker(&worker);
        Ok((policy, worker, models))
    }
    /// Prepares the session as a request with the policy's own provider. Every
    /// policy in this file is built with the stateless `Script::normal()`.
    async fn prepare(&self, policy: &SummarizingContext) -> Result<PreparedContext> {
        prepare_fresh(
            policy,
            Script::normal(),
            &self.session,
            self.request(self.store.messages(&self.session)?),
        )
        .await
    }
    fn save_episodes(
        &self,
        history: &[Message],
        count: usize,
        summary_tokens: usize,
        closed: bool,
    ) -> Result<()> {
        let mut chapter = ContextSubjectChapter {
            id: id(),
            session_id: self.session.clone(),
            ordinal: 1,
            first_episode_ordinal: 1,
            last_episode_ordinal: 1,
            status: SubjectChapterStatus::Open,
            description: None,
            summary: None,
            related_ids: vec![],
            related_evidence: vec![],
            decisions: None,
            sections: None,
            model: None,
            usage: None,
            reasoning: None,
        };
        for i in 0..count {
            chapter.last_episode_ordinal = i + 1;
            let episode = ContextEpisode {
                id: id(),
                session_id: self.session.clone(),
                ordinal: i + 1,
                first_message_id: history[2 * i].id.clone(),
                last_message_id: history[2 * i + 1].id.clone(),
                source_tokens: 35000,
                cap: 2200,
                origin_request_id: None,
                title: format!("title {i}"),
                summary: format!("summary {i}{}", " datum".repeat(summary_tokens)),
                model: "fake".into(),
                usage: None,
                reasoning: None,
                created_at: now(),
                chapter_id: chapter.id.clone(),
            };
            let close_after_episode = if closed && i + 1 == count {
                let mut c = chapter.clone();
                c.status = SubjectChapterStatus::Closed;
                c.description = Some("subject".into());
                c.summary = Some("FUTURE_CHAPTER_SECRET".into());
                c.model = Some("fake".into());
                Some(c)
            } else {
                None
            };
            self.store.save_episode_batch(&EpisodeBatch {
                episode,
                decisions: vec![],
                open_chapter: chapter.clone(),
                closed_chapter: None,
                close_after_episode,
                expected_previous_coverage: self.store.episode_coverage_token(&self.session)?,
            })?;
        }
        Ok(())
    }
}
#[test]
fn threshold_reserves_sixteen_thousand_tokens_and_metadata_stays_explicit() -> Result<()> {
    assert_eq!(SummaryConfig::default().threshold(), 184_000);
    assert_eq!(
        SummaryConfig {
            context_window: Some(400_000),
            ..Default::default()
        }
        .threshold(),
        184_000
    );
    assert_eq!(
        SummaryConfig {
            context_window: Some(200_000),
            ..Default::default()
        }
        .threshold(),
        184_000
    );
    assert_eq!(
        SummaryConfig {
            context_window: Some(16_101),
            threshold_cap: 20_000,
            ..Default::default()
        }
        .threshold(),
        101
    );
    assert_eq!(
        SummaryConfig {
            context_window: Some(272_000),
            ..Default::default()
        }
        .threshold(),
        184_000
    );
    assert_eq!(
        SummaryConfig {
            context_window: Some(16_000),
            ..Default::default()
        }
        .threshold(),
        0
    );
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("models.json");
    assert_eq!(cached_context_window(&path, "known"), None);
    std::fs::write(&path, json!({"models":[{"slug":"known","context_window":200000},{"slug":"zero","context_window":0},{"slug":"bad","context_window":"200000"}]}).to_string())?;
    assert_eq!(cached_context_window(&path, "known"), Some(200_000));
    for model in ["unknown", "zero", "bad"] {
        assert_eq!(cached_context_window(&path, model), None);
    }
    std::fs::write(&path, "invalid json")?;
    assert_eq!(cached_context_window(&path, "known"), None);
    Ok(())
}

#[test]
fn encrypted_reasoning_is_opaque_and_responses_wire_content_is_not_duplicated() -> Result<()> {
    let fixture = Fixture::new()?;
    let policy = fixture.policy(Script::normal(), 1200)?;
    let mut assistant = fixture.message(Role::Assistant, "Visible answer.");
    assistant.provider_state = Some(
        json!({"adapter":"responses","items":[{"type":"reasoning","summary":[],"encrypted_content":"short"},{"type":"message","content":[{"type":"output_text","text":"Visible answer."}]}]}),
    );
    let short = policy.estimated_tokens(&fixture.request(vec![assistant.clone()]));
    assistant.content = "duplicated visible content ".repeat(2000);
    assistant.provider_state.as_mut().unwrap()["items"][0]["encrypted_content"] =
        json!("opaque ciphertext ".repeat(10000));
    assert_eq!(
        policy.estimated_tokens(&fixture.request(vec![assistant])),
        short
    );
    Ok(())
}

#[tokio::test]
async fn lcm_rebuilds_compacted_view_after_serving_wipe_without_summary_or_main_calls() -> Result<()>
{
    let fixture = Fixture::new()?;
    fixture.seed(&fixture.turns(4, 35000))?;
    let (policy, worker, models) = fixture.worker(100000)?;
    let first = fixture.prepare(&policy).await?;
    assert!(first.notice.is_some());
    let calls = models.calls.load(Ordering::SeqCst);
    assert!(calls > 0);
    let history = serde_json::to_vec(&fixture.store.messages(&fixture.session)?)?;
    let episodes = fixture.store.context_episodes(&fixture.session)?;
    assert!(!episodes.is_empty());
    assert!(fixture.store.context_view(&fixture.session)?.is_some());
    drop(worker);
    drop(policy);

    fixture.store.serving_wipe()?;
    assert!(fixture.store.context_view(&fixture.session)?.is_none());
    let restarted = fixture.policy(Script::normal(), 100000)?;
    let recovered = fixture.prepare(&restarted).await?;
    assert!(restarted.estimated_tokens(&recovered.request) < 100000);
    assert_eq!(models.calls.load(Ordering::SeqCst), calls);
    assert_eq!(
        serde_json::to_vec(&fixture.store.messages(&fixture.session)?)?,
        history
    );
    let recovered_episodes = fixture.store.context_episodes(&fixture.session)?;
    assert_eq!(recovered_episodes.len(), episodes.len());
    for (before, after) in episodes.iter().zip(&recovered_episodes) {
        assert_eq!(
            (
                &before.id,
                &before.summary,
                &before.first_message_id,
                &before.last_message_id
            ),
            (
                &after.id,
                &after.summary,
                &after.first_message_id,
                &after.last_message_id
            )
        );
    }
    // Rebuilding creates fresh synthetic message IDs/timestamps; the ordered
    // model-facing content must still match the original selected view.
    let content = |request: &ModelRequest| {
        request.messages.iter().map(|message| json!({"role":message.role,"content":message.content,"tool_calls":message.tool_calls,"tool_call_id":message.tool_call_id,"provider_state":message.provider_state,"is_error":message.is_error})).collect::<Vec<_>>()
    };
    assert_eq!(content(&first.request), content(&recovered.request));
    assert_eq!(first.request.system_prompt, recovered.request.system_prompt);
    assert_eq!(
        serde_json::to_vec(&fixture.prepare(&restarted).await?.request)?,
        serde_json::to_vec(&recovered.request)?
    );
    Ok(())
}

#[tokio::test]
async fn catchup_publishes_frozen_prefix_and_worker_saves_do_not_rewrite_provider_input()
-> Result<()> {
    let f = Fixture::new()?;
    let original = f.turns(4, 35000);
    f.seed(&original)?;
    let (policy, worker, models) = f.worker(100000)?;
    let first = f.prepare(&policy).await?;
    assert!(first.notice.is_some());
    assert_eq!(models.calls.load(Ordering::SeqCst), 2);
    assert!(f.store.context_chapters(&f.session)?.is_empty());
    let stored = f.store.context_view(&f.session)?.unwrap();
    assert_eq!(
        stored.frontier,
        Some(f.store.message_seq(&f.session, &original[3].id)?)
    );
    assert_eq!(stored.budget, 100000);
    assert!(stored.tokens < 80000);
    let first_bytes = serde_json::to_vec(&first.request.messages)?;
    f.seed(&f.turns(1, 1000))?;
    worker.trigger(&f.session, 70000, 100000);
    worker.wait_idle(&f.session).await;
    assert_eq!(models.calls.load(Ordering::SeqCst), 4);
    let next = f.prepare(&policy).await?;
    assert!(next.notice.is_none());
    assert_eq!(
        serde_json::to_vec(&next.request.messages[..first.request.messages.len()])?,
        first_bytes
    );
    assert_eq!(
        serde_json::to_vec(&f.store.context_view(&f.session)?.unwrap())?,
        serde_json::to_vec(&stored)?
    );
    let reopened = Store::open(&f.path)?;
    let restarted = SummarizingContext::new(
        reopened,
        Script::normal(),
        SummaryConfig {
            threshold_cap: 100000,
            ..Default::default()
        },
        None,
    )?;
    assert_eq!(
        serde_json::to_vec(&f.prepare(&restarted).await?.request)?,
        serde_json::to_vec(&next.request)?
    );
    assert_eq!(
        serde_json::to_vec(&f.store.messages(&f.session)?[..original.len()])?,
        serde_json::to_vec(&original)?
    );
    for _ in 0..3 {
        f.seed(&f.turns(1, 1000))?;
        worker.trigger(&f.session, 80000, 100000);
        worker.wait_idle(&f.session).await;
        let step = f.prepare(&policy).await?;
        assert!(step.notice.is_none());
        assert_eq!(
            serde_json::to_vec(&step.request.messages[..first.request.messages.len()])?,
            first_bytes
        );
    }
    let mut changed = f.request(f.store.messages(&f.session)?);
    changed.system_prompt = "New host instructions".into();
    let current = prepare_fresh(&*policy, Script::normal(), &f.session, changed.clone()).await?;
    assert_eq!(current.request.system_prompt, "New host instructions");
    f.seed(&f.turns(1, 35000))?;
    changed.messages = f.store.messages(&f.session)?;
    let compacted = prepare_fresh(&*policy, Script::normal(), &f.session, changed).await?;
    assert!(compacted.notice.is_some());
    assert_eq!(compacted.request.system_prompt, "New host instructions");
    assert_ne!(
        f.store.context_view(&f.session)?.unwrap().created_at,
        stored.created_at
    );
    let canonical_request = f.request(f.store.messages(&f.session)?);
    let expected = prepare_fresh(
        &*policy,
        Script::normal(),
        &f.session,
        canonical_request.clone(),
    )
    .await?
    .request;
    worker.wait_idle(&f.session).await;
    drop(restarted);
    drop(policy);
    drop(worker);
    drop(models);
    let Fixture {
        _directory,
        path,
        store,
        session,
    } = f;
    drop(store);
    // Reopen after every owner of the original SQLite connection has closed.
    let restored = SummarizingContext::new(
        Store::open(path)?,
        Script::normal(),
        SummaryConfig {
            threshold_cap: 100000,
            ..Default::default()
        },
        None,
    )?;
    let actual = prepare_fresh(&restored, Script::normal(), &session, canonical_request)
        .await?
        .request;
    assert_eq!(serde_json::to_vec(&actual)?, serde_json::to_vec(&expected)?);
    Ok(())
}
#[tokio::test]
async fn catchup_and_background_model_work_are_serialized() -> Result<()> {
    let f = Fixture::new()?;
    f.seed(&f.turns(5, 35000))?;
    let (policy, worker, models) = f.worker(100000)?;
    models.pause.store(true, Ordering::SeqCst);
    worker.trigger(&f.session, 175000, 100000);
    models.entered.notified().await;
    let task = tokio::spawn({
        let policy = policy.clone();
        let session = f.session.clone();
        let request = f.request(f.store.messages(&f.session)?);
        async move { prepare_fresh(&*policy, Script::normal(), &session, request).await }
    });
    tokio::task::yield_now().await;
    models.release.notify_one();
    task.await??;
    worker.wait_idle(&f.session).await;
    assert_eq!(models.peak.load(Ordering::SeqCst), 1);
    assert_eq!(models.calls.load(Ordering::SeqCst), 5);
    Ok(())
}
#[tokio::test]
async fn ready_coverage_applies_without_waiting_for_a_newer_summary() -> Result<()> {
    let f = Fixture::new()?;
    let history = f.turns(5, 35000);
    f.seed(&history)?;
    f.save_episodes(&history, 3, 0, false)?;
    let (policy, worker, models) = f.worker(100000)?;
    models.pause.store(true, Ordering::SeqCst);
    worker.trigger(&f.session, 175000, 100000);
    models.entered.notified().await;
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), f.prepare(&policy)).await;
    // Release even on a regression so the test never leaves a detached request.
    models.release.notify_one();
    worker.wait_idle(&f.session).await;
    let prepared = result.expect("ready coverage must not wait for the paused newer summary")?;
    assert!(prepared.notice.is_some());
    assert!(policy.estimated_tokens(&prepared.request) < 100000);
    assert_eq!(f.store.messages(&f.session)?.len(), history.len());
    Ok(())
}

#[tokio::test]
async fn apply_serves_ready_episodes_and_exact_recent_tools_without_refilling_old_history()
-> Result<()> {
    let f = Fixture::new()?;
    let history = f.tool_rounds(6, 12000);
    f.seed(&history)?;
    f.save_episodes(&history, 4, 30, false)?;
    let mut config = SummaryConfig {
        threshold_cap: 60000,
        ..Default::default()
    };
    config.lcm.raw_tail_tokens = 16000;
    let policy = Arc::new(SummarizingContext::new(
        f.store.clone(),
        Script::normal(),
        config,
        None,
    )?);
    let models = Arc::new(Models::default());
    let worker = new_episode_worker(f.store.clone(), models, policy.clone());
    policy.set_episode_worker(&worker);
    let prepared = f.prepare(&policy).await?;
    assert!(prepared.notice.is_some());
    let view = f.store.context_view(&f.session)?.unwrap();
    assert_eq!(
        view.frontier,
        Some(f.store.message_seq(&f.session, &history[7].id)?)
    );
    assert!(view.masked_tool_output_ids.is_empty());
    assert!(view.clipped_tool_call_ids.is_empty());
    let prefix = view
        .prefix
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    for index in 0..4 {
        assert!(prefix.contains(&format!("summary {index}")));
    }
    for original in &history[..8] {
        assert!(
            !prepared
                .request
                .messages
                .iter()
                .any(|m| m.id == original.id)
        );
    }
    for original in &history[8..] {
        assert_eq!(
            prepared
                .request
                .messages
                .iter()
                .find(|m| m.id == original.id),
            Some(original)
        );
    }
    assert_eq!(f.store.messages(&f.session)?, history);

    // Ready background memory cannot rewrite an already served prefix.
    let old_request = serde_json::to_vec(&prepared.request.messages)?;
    worker.trigger(&f.session, 0, 0);
    worker.wait_idle(&f.session).await;
    assert!(f.store.context_episodes(&f.session)?.len() > 4);
    let addition = f.turns(1, 1000);
    f.seed(&addition)?;
    let next = f.prepare(&policy).await?;
    assert!(next.notice.is_none());
    assert_eq!(
        serde_json::to_vec(&next.request.messages[..prepared.request.messages.len()])?,
        old_request
    );
    assert_eq!(
        f.store.context_view(&f.session)?.unwrap().created_at,
        view.created_at
    );

    // Verify the actual Chat serializer as well as the stored Rust view. This
    // loopback endpoint does not model upstream cache behavior or run an LLM.
    let captured: Arc<std::sync::Mutex<Vec<serde_json::Value>>> = Arc::default();
    let app = axum::Router::new()
        .route(
            "/chat/completions",
            axum::routing::post(
                |axum::extract::State(captured): axum::extract::State<
                    Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
                >,
                 axum::Json(body): axum::Json<serde_json::Value>| async move {
                    captured.lock().unwrap().push(body);
                    axum::Json(
                        json!({"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":1,"completion_tokens":1}}),
                    )
                },
            ),
        )
        .with_state(captured.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let provider =
        bubblepaw_runner::provider::ChatCompletionsProvider::api_key("synthetic-key", endpoint)?;
    for mut request in [prepared.request.clone(), next.request.clone()] {
        request.tools.push(bubblepaw_core::ToolDefinition {
            name: "js".into(),
            description: "Execute a workspace cell".into(),
            input_schema: json!({"type":"object","properties":{"code":{"type":"string"}},"required":["code"]}),
            read_only: false,
            approval: Default::default(),
        });
        provider
            .generate(request, Arc::new(|_| {}), CancellationToken::new())
            .await?;
    }
    server.abort();
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let first_messages = requests[0]["messages"].as_array().unwrap();
    assert_eq!(
        &requests[1]["messages"].as_array().unwrap()[..first_messages.len()],
        first_messages
    );
    let mut first_outer = requests[0].clone();
    let mut next_outer = requests[1].clone();
    first_outer.as_object_mut().unwrap().remove("messages");
    next_outer.as_object_mut().unwrap().remove("messages");
    assert_eq!(first_outer, next_outer);
    Ok(())
}

#[tokio::test]
async fn physical_window_pressure_applies_closed_and_open_memory_with_exact_uncovered_suffix()
-> Result<()> {
    let f = Fixture::new()?;
    let mut history = f.turns(3, 24_000);
    history.extend(f.turns(1, 6_000));
    f.seed(&history)?;
    f.save_episodes(&history, 2, 2000, false)?;
    let mut closed = f.store.context_subject_chapters(&f.session)?[0].clone();
    closed.status = SubjectChapterStatus::Closed;
    closed.description = Some("Completed first phase".into());
    closed.model = Some("fake".into());
    closed.summary = Some(format!("CLOSED_E1_E2{}", " datum".repeat(5_000)));
    // Persist the replacement as part of the next atomic coverage batch.
    let mut third = f.store.context_episodes(&f.session)?[1].clone();
    third.id = id();
    third.ordinal = 3;
    third.first_message_id = history[4].id.clone();
    third.last_message_id = history[5].id.clone();
    third.summary = format!("OPEN_E3{}", " datum".repeat(2_000));
    let mut open = closed.clone();
    open.id = id();
    open.ordinal = 2;
    open.first_episode_ordinal = 3;
    open.last_episode_ordinal = 3;
    open.status = SubjectChapterStatus::Open;
    open.description = None;
    open.summary = None;
    open.model = None;
    open.usage = None;
    open.reasoning = None;
    third.chapter_id = open.id.clone();
    f.store.save_episode_batch(&EpisodeBatch {
        episode: third,
        decisions: vec![],
        open_chapter: open,
        closed_chapter: Some(closed),
        close_after_episode: None,
        expected_previous_coverage: f.store.episode_coverage_token(&f.session)?,
    })?;
    let policy = SummarizingContext::new(
        f.store.clone(),
        Script::normal(),
        SummaryConfig {
            context_window: Some(100_000),
            ..Default::default()
        },
        None,
    )?;
    let mut original = f.request(history.clone());
    original.system_prompt = " stable".repeat(8_000);
    original.tools.push(bubblepaw_core::ToolDefinition {
        name: "history_read".into(),
        description: "Read original source".into(),
        input_schema: json!({"type":"object"}),
        read_only: true,
        approval: Default::default(),
    });
    assert!(policy.estimated_tokens(&original) > 84_000);
    let prepared = prepare_fresh(&policy, Script::normal(), &f.session, original.clone()).await?;
    assert!(prepared.notice.is_some());
    assert_eq!(prepared.request.system_prompt, original.system_prompt);
    assert_eq!(
        serde_json::to_value(&prepared.request.tools)?,
        serde_json::to_value(&original.tools)?
    );
    let view = f.store.context_view(&f.session)?.unwrap();
    assert_eq!(
        view.frontier,
        Some(f.store.message_seq(&f.session, &history[5].id)?)
    );
    assert_eq!(
        &prepared.request.messages[view.prefix.len()..],
        &history[6..]
    );
    assert_eq!(prepared.request.messages.first(), Some(&history[0]));
    let memory = view
        .prefix
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(memory.contains("CLOSED_E1_E2"));
    assert!(memory.contains("OPEN_E3"));
    assert!(!memory.contains("summary 0"));
    assert!(!memory.contains("summary 1"));
    assert!(policy.estimated_tokens(&prepared.request) < 25_000);
    assert_eq!(f.store.messages(&f.session)?, history);
    let again = prepare_fresh(&policy, Script::normal(), &f.session, original).await?;
    assert!(again.notice.is_none());
    assert_eq!(again.request.messages, prepared.request.messages);
    Ok(())
}

#[tokio::test]
async fn uncovered_tool_history_cannot_be_masked_into_a_fake_summary_apply() -> Result<()> {
    let f = Fixture::new()?;
    let history = f.tool_rounds(6, 12000);
    f.seed(&history)?;
    let (policy, _worker, models) = f.worker(60000)?;
    models.fail.store(true, Ordering::SeqCst);
    let error = match f.prepare(&policy).await {
        Err(error) => error,
        Ok(_) => panic!("uncovered tool results need a real summary"),
    };
    assert!(error.to_string().contains("Context summarization failed"));
    assert!(f.store.context_view(&f.session)?.is_none());
    assert_eq!(f.store.messages(&f.session)?, history);
    Ok(())
}

#[tokio::test]
async fn pressure_fallback_cannot_summarize_an_incomplete_call_group() -> Result<()> {
    let f = Fixture::new()?;
    let mut history = f.turns(2, 20000);
    let mut pending = f.message(Role::Assistant, "");
    pending.tool_calls.push(ToolCall {
        id: "unfinished-effect".into(),
        name: "js".into(),
        arguments: json!({"code":" datum".repeat(70000)}),
    });
    history.push(pending);
    f.seed(&history)?;
    f.save_episodes(&history, 2, 0, false)?;
    let (policy, _worker, models) = f.worker(60000)?;
    let error = match f.prepare(&policy).await {
        Err(error) => error,
        Ok(_) => panic!("the unfinished tool call cannot be replaced by a summary"),
    };
    assert!(error.to_string().contains("retained recent input"));
    assert_eq!(models.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.store.context_episodes(&f.session)?.len(), 2);
    assert!(f.store.context_view(&f.session)?.is_none());
    assert_eq!(f.store.messages(&f.session)?, history);
    Ok(())
}

#[tokio::test]
async fn child_assignment_and_latest_steering_survive_a_bounded_compacted_view() -> Result<()> {
    let f = Fixture::new()?;
    let mut history = f.turns(5, 35000);
    history[0].origin = Some(bubblepaw_core::MessageOrigin::ChildTask);
    history[0].content = "Implement the requested fix; preserve exact failure evidence.".into();
    for index in [2, 4, 6, 8] {
        history[index].origin = Some(bubblepaw_core::MessageOrigin::ChildTask);
    }
    history[8].content = "Latest steering: keep the public API compatible.".into();
    f.seed(&history)?;
    f.save_episodes(&history, 5, 0, false)?;
    let mut config = SummaryConfig {
        threshold_cap: 100000,
        ..Default::default()
    };
    config.lcm.raw_tail_tokens = 16000;
    let policy = SummarizingContext::new(f.store.clone(), Script::normal(), config, None)?;
    let prepared = f.prepare(&policy).await?;
    let assignment = prepared
        .request
        .messages
        .iter()
        .find(|m| m.id == history[0].id)
        .unwrap();
    assert_eq!(assignment.content, history[0].content);
    assert_eq!(
        assignment.origin,
        Some(bubblepaw_core::MessageOrigin::ChildTask)
    );
    let steering = prepared
        .request
        .messages
        .iter()
        .find(|m| m.id == history[8].id)
        .unwrap();
    assert_eq!(steering.content, history[8].content);
    assert!(!bubblepaw_core::is_typed_user(assignment));
    assert_eq!(f.store.messages(&f.session)?.len(), history.len());
    Ok(())
}
#[tokio::test]
async fn catchup_failure_uses_saved_coverage_or_leaves_old_view_when_it_cannot_fit() -> Result<()> {
    let f = Fixture::new()?;
    let history = f.turns(5, 35000);
    f.seed(&history)?;
    f.save_episodes(&history, 4, 0, false)?;
    let (policy, _worker, models) = f.worker(100000)?;
    models.fail.store(true, Ordering::SeqCst);
    assert!(f.prepare(&policy).await?.notice.is_some());
    let old = serde_json::to_vec(&f.store.context_view(&f.session)?)?;
    f.seed(&f.turns(3, 35000))?;
    let error = f.prepare(&policy).await.err().unwrap();
    assert!(error.to_string().contains("gathering failed"));
    assert_eq!(old, serde_json::to_vec(&f.store.context_view(&f.session)?)?);
    assert!(models.calls.load(Ordering::SeqCst) > 0);
    let empty = Fixture::new()?;
    empty.seed(&empty.turns(4, 35000))?;
    let (policy, _worker, models) = empty.worker(100000)?;
    models.fail.store(true, Ordering::SeqCst);
    assert!(empty.prepare(&policy).await.is_err());
    assert!(empty.store.context_view(&empty.session)?.is_none());
    Ok(())
}
#[tokio::test]
async fn pressure_consumes_whole_ready_closed_chapter_regardless_of_old_raw_tail_target()
-> Result<()> {
    let f = Fixture::new()?;
    let history = f.turns(8, 35000);
    f.seed(&history)?;
    f.save_episodes(&history, 7, 6000, true)?;
    let mut config = SummaryConfig {
        threshold_cap: 256000,
        ..Default::default()
    };
    config.lcm.raw_tail_tokens = 100000;
    let policy = SummarizingContext::new(f.store.clone(), Script::normal(), config, None)?;
    let prepared = f.prepare(&policy).await?;
    let view = f.store.context_view(&f.session)?.unwrap();
    let frontier = f
        .store
        .context_view_frontier_message_id(&f.session, view.frontier)?
        .unwrap();
    let boundary = history.iter().position(|m| m.id == frontier).unwrap();
    let prefix = view
        .prefix
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(prefix.contains("FUTURE_CHAPTER_SECRET"));
    assert!(!prefix.contains("Partial chapter"));
    for i in 0..7 {
        if 2 * i + 1 > boundary {
            assert!(!prefix.contains(&format!("summary {i}")));
        }
    }
    assert!(!prefix.contains("title 0"));
    let tokenizer = tiktoken_rs::o200k_base_singleton();
    let bodies = view
        .prefix
        .iter()
        .find(|m| m.content.starts_with("Closed chapter summaries"))
        .unwrap();
    assert!(tokenizer.encode_ordinary(&bodies.content).len() < 16100);
    assert!(policy.estimated_tokens(&prepared.request) <= 128000);
    Ok(())
}
#[tokio::test]
async fn open_chapter_keeps_every_complete_summary_instead_of_only_older_titles() -> Result<()> {
    let f = Fixture::new()?;
    let history = f.turns(8, 35000);
    f.seed(&history)?;
    f.save_episodes(&history, 7, 9000, false)?;
    let policy = f.policy(Script::normal(), 256000)?;
    f.prepare(&policy).await?;
    let view = f.store.context_view(&f.session)?.unwrap();
    let block = view
        .prefix
        .iter()
        .find(|m| m.content.starts_with("Open chapter episodes"))
        .unwrap();
    assert!(block.content.contains("title 0"));
    assert!(block.content.contains("summary 0"));
    assert!(
        tiktoken_rs::o200k_base_singleton()
            .encode_ordinary(&block.content)
            .len()
            > 63000
    );
    Ok(())
}
#[tokio::test]
async fn uncovered_results_and_arguments_stay_exact_with_responses_reasoning() -> Result<()> {
    let f = Fixture::new()?;
    let mut history = f.turns(8, 35000);
    let arguments = json!({"code":"x".repeat(3000)});
    let mut call = f.message(Role::Assistant, "");
    call.tool_calls = vec![ToolCall {
        id: "huge-call".into(),
        name: "js".into(),
        arguments: arguments.clone(),
    }];
    call.provider_state = Some(
        json!({"adapter":"responses","items":[{"type":"reasoning","encrypted_content":"opaque"},{"type":"function_call","call_id":"huge-call","name":"js","arguments":arguments.to_string()}]}),
    );
    let mut output = f.message(Role::Tool, "tool raw ".repeat(100));
    output.tool_call_id = Some("huge-call".into());
    history.splice(12..12, [call.clone(), output.clone()]);
    f.seed(&history)?;
    // Explicit episode ranges stop before the inserted old raw round.
    f.save_episodes(&history[..12], 6, 0, false)?;
    let policy = f.policy(Script::normal(), 256000)?;
    let prepared = f.prepare(&policy).await?;
    let view = f.store.context_view(&f.session)?.unwrap();
    assert!(view.masked_tool_output_ids.is_empty());
    assert!(view.clipped_tool_call_ids.is_empty());
    let shown = prepared
        .request
        .messages
        .iter()
        .find(|m| m.id == output.id)
        .unwrap();
    assert_eq!(shown.content, output.content);
    let shown = prepared
        .request
        .messages
        .iter()
        .find(|m| m.id == call.id)
        .unwrap();
    let state = shown.provider_state.as_ref().unwrap();
    assert_eq!(state["items"][0]["encrypted_content"], "opaque");
    assert_eq!(
        state["items"][1]["arguments"],
        shown.tool_calls[0].arguments.to_string()
    );
    assert_eq!(shown.tool_calls[0].arguments, arguments);
    let tail = history.last().unwrap();
    assert_eq!(
        serde_json::to_value(
            prepared
                .request
                .messages
                .iter()
                .find(|m| m.id == tail.id)
                .unwrap()
        )?,
        serde_json::to_value(tail)?
    );
    let mut registry = bubblepaw_core::ToolRegistry::new();
    bubblepaw_runner::tools::register_history_tools(&mut registry, f.store.clone())?;
    let context = bubblepaw_core::ToolContext {
        session_id: f.session.clone(),
        run_id: "read-test".into(),
        invocation_id: id(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    let result = registry
        .get_host("history_read")
        .unwrap()
        .invoke(context.clone(), json!({"message_id":output.id}))
        .await?;
    let recovered: serde_json::Value = serde_json::from_str(&result.content)?;
    assert_eq!(recovered["messages"][0]["text"], output.content);
    let result = registry
        .get_host("history_read")
        .unwrap()
        .invoke(context, json!({"tool_call_id":"huge-call"}))
        .await?;
    let recovered: serde_json::Value = serde_json::from_str(&result.content)?;
    assert_eq!(
        recovered["messages"][0]["tool_calls"][0]["arguments"],
        arguments
    );
    let prefix_bytes = serde_json::to_vec(&prepared.request.messages)?;
    let mut appended = f.message(Role::Assistant, "");
    appended.tool_calls = vec![ToolCall {
        id: "new-call".into(),
        name: "js".into(),
        arguments: arguments.clone(),
    }];
    f.seed(&[appended.clone()])?;
    let next = f.prepare(&policy).await?;
    assert!(next.notice.is_none());
    assert_eq!(
        serde_json::to_vec(&next.request.messages[..prepared.request.messages.len()])?,
        prefix_bytes
    );
    assert_eq!(
        serde_json::to_value(next.request.messages.last().unwrap())?,
        serde_json::to_value(appended)?
    );
    Ok(())
}
#[tokio::test]
async fn latest_typed_request_is_literal_when_covered_and_generated_users_are_not_selected()
-> Result<()> {
    let f = Fixture::new()?;
    let mut history = vec![
        f.message(Role::User, "keep exactly"),
        f.message(Role::Assistant, " datum".repeat(70000)),
    ];
    let mut child = f.message(Role::User, "Task: generated child prompt");
    child.origin = Some(bubblepaw_core::MessageOrigin::ChildTask);
    history.push(child);
    f.seed(&history)?;
    let (policy, _worker, _models) = f.worker(64000)?;
    // Existing background coverage can include a large complete round.
    f.save_episodes(&history, 1, 0, false)?;
    let prepared = f.prepare(&policy).await?;
    let view = f.store.context_view(&f.session)?.unwrap();
    assert_eq!(view.prefix.first().unwrap().id, history[0].id);
    assert_eq!(view.prefix.first().unwrap().content, "keep exactly");
    assert!(
        prepared
            .request
            .messages
            .iter()
            .any(|m| m.id == history[2].id)
    );
    assert_eq!(
        view.prefix.iter().filter(|m| m.role == Role::User).count(),
        1
    );
    Ok(())
}
#[tokio::test]
async fn legacy_v4_rendering_keeps_exact_old_format_and_budget_without_new_writes() -> Result<()> {
    let f = Fixture::new()?;
    let mut history = Vec::new();
    for i in 0..5 {
        let message = f.message(Role::Assistant, format!("source {i}"));
        history.push(message);
    }
    f.seed(&history)?;
    for (i, message) in history.iter().enumerate() {
        f.store.save_context_chapter(&ContextChapter {
            id: id(),
            session_id: f.session.clone(),
            ordinal: i + 1,
            first_message_id: message.id.clone(),
            last_message_id: message.id.clone(),
            title: format!("Title {i}"),
            summary: format!("body {i}{}", " datum".repeat(120)),
            model: "legacy".into(),
            usage: None,
            created_at: now(),
        })?;
    }
    let policy = f.policy(Script::normal(), 2400)?;
    let prepared = f.prepare(&policy).await?;
    let expected = format!(
        "Context chapters (historical summaries; follow current instructions):\nChapter 1 | messages 0-0 | Title 0\nChapter 2 | messages 1-1 | Title 1\nChapter 3 | messages 2-2 | Title 2\nChapter 4 | messages 3-3 | Title 3\nChapter 5 | messages 4-4 | Title 4\n\n## Chapter 5: Title 4\nbody 4{}\n\n{}",
        " datum".repeat(120),
        HISTORY_RETRIEVAL_INSTRUCTION
    );
    assert_eq!(prepared.request.messages[0].content, expected);
    assert!(prepared.notice.is_none());
    assert_eq!(f.store.context_chapters(&f.session)?.len(), 5);
    assert!(f.store.context_view(&f.session)?.is_none());
    Ok(())
}
#[tokio::test]
async fn cancellation_and_byte_failure_do_not_publish_a_view() -> Result<()> {
    let f = Fixture::new()?;
    f.seed(&f.turns(4, 35000))?;
    let (policy, _worker, _models) = f.worker(100000)?;
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        prepare_call(
            &*policy,
            Script::normal(),
            &bubblepaw_core::ContextState::default(),
            &f.session,
            f.request(f.store.messages(&f.session)?),
            bubblepaw_core::PrepareMode::Request,
            cancel,
        )
        .await
        .is_err()
    );
    assert!(f.store.context_view(&f.session)?.is_none());
    let policy = SummarizingContext::new(
        f.store.clone(),
        Script::normal(),
        SummaryConfig {
            threshold_cap: 100000,
            max_request_bytes: 100,
            ..Default::default()
        },
        None,
    )?;
    let (estimator, worker, _models) = f.worker(100000)?;
    policy.set_episode_worker(&worker);
    assert!(f.prepare(&policy).await.is_err());
    assert!(f.store.context_view(&f.session)?.is_none());
    drop(estimator);
    Ok(())
}
#[tokio::test]
async fn usage_calibration_does_not_accumulate_run_totals_or_follow_worker_saves() -> Result<()> {
    let f = Fixture::new()?;
    f.seed(&[f.message(Role::User, "short")])?;
    let policy = f.policy(Script::normal(), 100000)?;
    for _ in 0..12 {
        assert!(f.prepare(&policy).await?.notice.is_none());
        policy.observe_response(
            &f.session,
            &ModelResponse {
                text: "answer".into(),
                usage: Some(Usage {
                    input_tokens: 100,
                    output_tokens: 1,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
    }
    assert!(f.store.context_view(&f.session)?.is_none());
    Ok(())
}

#[tokio::test]
async fn usage_calibration_uses_final_physical_request_and_clears_unknown_anchor() -> Result<()> {
    let f = Fixture::new()?;
    f.seed(&[f.message(Role::User, "short")])?;
    let policy = f.policy(Script::normal(), 100000)?;
    f.prepare(&policy).await?;
    let response = ModelResponse {
        text: "answer".into(),
        usage: Some(Usage {
            input_tokens: 150000,
            output_tokens: 160000,
            ..Default::default()
        }),
        last_request_usage: Some(bubblepaw_core::LastRequestUsage {
            input_tokens: Some(100),
            output_tokens: Some(1),
        }),
        ..Default::default()
    };
    policy.observe_response(&f.session, &response);
    f.seed(&[f.message(Role::Assistant, "answer")])?;
    assert!(f.prepare(&policy).await?.notice.is_none());
    assert!(f.store.context_view(&f.session)?.is_none());

    // Unknown physical input must not reuse aggregate billing or an older
    // anchor. Known physical input remains usable when output is unreported;
    // new native output is counted in the next rendered input instead.
    for last in [
        bubblepaw_core::LastRequestUsage {
            input_tokens: None,
            output_tokens: Some(1),
        },
        bubblepaw_core::LastRequestUsage {
            input_tokens: Some(100),
            output_tokens: None,
        },
    ] {
        policy.observe_response(
            &f.session,
            &ModelResponse {
                usage: Some(Usage {
                    input_tokens: 200000,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        policy.observe_response(
            &f.session,
            &ModelResponse {
                last_request_usage: Some(last),
                ..response.clone()
            },
        );
        assert!(f.prepare(&policy).await?.notice.is_none());
        assert!(f.store.context_view(&f.session)?.is_none());
    }
    Ok(())
}
#[tokio::test]
async fn insufficient_ready_coverage_times_out_without_publishing_unfinished_models() -> Result<()>
{
    let f = Fixture::new()?;
    let history = f.turns(5, 35000);
    f.seed(&history)?;
    f.save_episodes(&history, 3, 0, false)?;
    let policy = Arc::new(SummarizingContext::new(
        f.store.clone(),
        Script::normal(),
        SummaryConfig {
            threshold_cap: 60000,
            timeout: std::time::Duration::from_millis(5),
            ..Default::default()
        },
        None,
    )?);
    let models = Arc::new(Models::default());
    models.pause.store(true, Ordering::SeqCst);
    let worker = new_episode_worker(f.store.clone(), models.clone(), policy.clone());
    policy.set_episode_worker(&worker);
    // Establish an unfinished model call before starting the short apply
    // deadline. Source scanning/tokenization may itself exceed five wall-clock
    // milliseconds; that must not make this a race over whether a call began.
    worker.trigger(&f.session, 175000, 60000);
    models.entered.notified().await;
    let result = f.prepare(&policy).await;
    let count_before_release = f.store.context_episodes(&f.session)?.len();
    let calls_before_release = models.calls.load(Ordering::SeqCst);
    let view_before_release = f.store.context_view(&f.session)?;
    models.release.notify_one();
    worker.wait_idle(&f.session).await;
    assert!(result.is_err());
    assert_eq!(count_before_release, 3);
    assert_eq!(calls_before_release, 1);
    assert!(view_before_release.is_none());
    assert!(f.store.context_view(&f.session)?.is_none());
    assert_eq!(f.store.messages(&f.session)?.len(), history.len());
    Ok(())
}
#[tokio::test]
async fn full_closed_chapter_summaries_are_chronological_without_silent_body_omissions()
-> Result<()> {
    let f = Fixture::new()?;
    let history = f.turns(8, 35000);
    f.seed(&history)?;
    for i in 0..7 {
        let chapter = ContextSubjectChapter {
            id: id(),
            session_id: f.session.clone(),
            ordinal: i + 1,
            first_episode_ordinal: i + 1,
            last_episode_ordinal: i + 1,
            status: SubjectChapterStatus::Open,
            description: None,
            summary: None,
            related_ids: vec![],
            related_evidence: vec![],
            decisions: None,
            sections: None,
            model: None,
            usage: None,
            reasoning: None,
        };
        let episode = ContextEpisode {
            id: id(),
            session_id: f.session.clone(),
            ordinal: i + 1,
            first_message_id: history[2 * i].id.clone(),
            last_message_id: history[2 * i + 1].id.clone(),
            source_tokens: 35000,
            cap: 2200,
            origin_request_id: None,
            title: format!("title {i}"),
            summary: format!("episode summary {i}"),
            model: "fake".into(),
            usage: None,
            reasoning: None,
            created_at: now(),
            chapter_id: chapter.id.clone(),
        };
        let mut closed = chapter.clone();
        closed.status = SubjectChapterStatus::Closed;
        closed.description = Some(format!("closed description {i}"));
        closed.summary = Some(format!("chapter body {i}{}", " datum".repeat(6000)));
        closed.model = Some("fake".into());
        f.store.save_episode_batch(&EpisodeBatch {
            episode,
            decisions: vec![],
            open_chapter: chapter,
            closed_chapter: None,
            close_after_episode: Some(closed),
            expected_previous_coverage: f.store.episode_coverage_token(&f.session)?,
        })?;
    }
    let policy = f.policy(Script::normal(), 256000)?;
    f.prepare(&policy).await?;
    let view = f.store.context_view(&f.session)?.unwrap();
    let frontier = f
        .store
        .context_view_frontier_message_id(&f.session, view.frontier)?
        .unwrap();
    let boundary = history.iter().position(|m| m.id == frontier).unwrap() / 2;
    let index = view
        .prefix
        .iter()
        .find(|m| m.content.starts_with("Subject chapter index"))
        .unwrap();
    for i in 0..=boundary {
        assert!(index.content.contains(&format!("closed description {i}")));
    }
    for i in boundary + 1..7 {
        assert!(!index.content.contains(&format!("closed description {i}")));
    }
    let body = view
        .prefix
        .iter()
        .find(|m| m.content.starts_with("Closed chapter summaries"))
        .unwrap();
    assert!(body.content.contains(&format!("chapter body {boundary}")));
    assert!(body.content.contains("chapter body 0"));
    assert!(
        tiktoken_rs::o200k_base_singleton()
            .encode_ordinary(&body.content)
            .len()
            > 42000
    );
    let newer = body
        .content
        .find(&format!("chapter body {boundary}"))
        .unwrap();
    let older = body
        .content
        .find(&format!("chapter body {}", boundary - 1))
        .unwrap();
    assert!(older < newer);
    for i in boundary + 1..7 {
        assert!(!body.content.contains(&format!("chapter body {i}")));
    }
    Ok(())
}
#[derive(Default)]
struct RecordingProvider {
    requests: std::sync::Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Provider for RecordingProvider {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request);
        }
        Ok(ModelResponse {
            text: " datum".repeat(1000),
            ..Default::default()
        })
    }
}
#[tokio::test]
async fn real_harness_prompts_keep_exact_provider_prefix_across_background_saves() -> Result<()> {
    let f = Fixture::new()?;
    f.seed(&f.turns(4, 35000))?;
    let provider = Arc::new(RecordingProvider::default());
    let artifacts = Arc::new(bubblepaw_runner::artifacts::ArtifactStore::new(
        f._directory.path(),
        f._directory.path().join("artifacts"),
    )?);
    let repl = Arc::new(bubblepaw_runner::codemode::CodeMode::new(
        f._directory.path().to_owned(),
        artifacts,
    )?);
    let notes = Arc::new(bubblepaw_runner::runtime_note::RuntimeNotes::without_processes(repl));
    let policy = Arc::new(SummarizingContext::new(
        f.store.clone(),
        provider.clone(),
        SummaryConfig {
            threshold_cap: 100000,
            ..Default::default()
        },
        Some(notes),
    )?);
    let models = Arc::new(Models::default());
    let worker = new_episode_worker(f.store.clone(), models, policy.clone());
    policy.set_episode_worker(&worker);
    let harness = bubblepaw_core::Harness::new(
        f.store.clone(),
        provider.clone(),
        bubblepaw_core::ToolRegistry::new(),
        bubblepaw_core::HarnessConfig {
            context: policy,
            ..Default::default()
        },
    );
    assert_eq!(
        harness
            .prompt(
                &f.session,
                "First live harness step",
                bubblepaw_core::RunOptions::default()
            )
            .await?
            .status,
        "completed"
    );
    let first = { provider.requests.lock().unwrap()[0].clone() };
    assert!(
        first
            .messages
            .iter()
            .all(|message| message.id != "ephemeral:runtime-note")
    );
    assert!(first.messages.iter().any(|message| {
        message.role == Role::User
            && message.origin == Some(bubblepaw_core::MessageOrigin::Generated)
            && message
                .content
                .contains("<harness kind=\"runtime_inventory\">")
    }));
    let durable_messages = |request: &ModelRequest| {
        request
            .messages
            .iter()
            .filter(|message| !bubblepaw_core::is_ephemeral(message))
            .cloned()
            .collect::<Vec<_>>()
    };
    let first_messages = durable_messages(&first);
    let first_bytes = serde_json::to_vec(&first_messages)?;
    let initial_view = f.store.context_view(&f.session)?.unwrap();
    assert!(
        initial_view
            .prefix
            .iter()
            .any(|m| m.id.starts_with("context:view:"))
    );
    assert!(
        initial_view
            .prefix
            .iter()
            .all(|m| !bubblepaw_core::is_ephemeral(m))
    );
    let coverage = f.store.context_episodes(&f.session)?.len();
    worker.trigger(&f.session, 70000, 100000);
    worker.wait_idle(&f.session).await;
    assert!(f.store.context_episodes(&f.session)?.len() > coverage);
    for i in 0..3 {
        assert_eq!(
            harness
                .prompt(
                    &f.session,
                    format!("Next harness step {i}"),
                    bubblepaw_core::RunOptions::default()
                )
                .await?
                .status,
            "completed"
        );
        let requests = provider.requests.lock().unwrap();
        let request = requests.last().unwrap();
        assert_eq!(request.system_prompt, first.system_prompt);
        assert_eq!(request.history.epoch, first.history.epoch);
        assert_eq!(request.history.rewritten_from, None);
        let messages = durable_messages(request);
        assert_eq!(
            serde_json::to_vec(&messages[..first_messages.len()])?,
            first_bytes
        );
    }
    f.seed(&f.turns(4, 35000))?;
    assert_eq!(
        harness
            .prompt(
                &f.session,
                "Compact again",
                bubblepaw_core::RunOptions::default()
            )
            .await?
            .status,
        "completed"
    );
    let compacted = provider.requests.lock().unwrap().last().unwrap().clone();
    assert_ne!(compacted.history.epoch, first.history.epoch);
    assert_ne!(
        f.store.context_view(&f.session)?.unwrap().created_at,
        initial_view.created_at
    );
    assert!(
        f.store
            .messages(&f.session)?
            .iter()
            .all(|m| !bubblepaw_core::is_ephemeral(m))
    );
    Ok(())
}
#[tokio::test]
async fn canonical_chat_journal_does_not_compact_but_active_byte_pressure_is_checked() -> Result<()>
{
    let f = Fixture::new()?;
    let mut archived = f.message(Role::Assistant, "visible");
    archived.provider_state = Some(json!({"adapter":"chat-completions",
        "message_fields":{"reasoning_content":"native"},
        "opaque_stream_fragments":[{"event":"x".repeat(3 * 1024 * 1024)}]}));
    f.seed(&[f.message(Role::User, "short"), archived.clone()])?;
    let policy = SummarizingContext::new(
        f.store.clone(),
        Script::normal(),
        SummaryConfig {
            threshold_cap: 100000,
            max_request_bytes: 4096,
            ..Default::default()
        },
        None,
    )?;
    let prepared = f.prepare(&policy).await?;
    assert!(prepared.notice.is_none());
    assert!(f.store.context_view(&f.session)?.is_none());
    assert_eq!(f.store.messages(&f.session)?[1], archived);
    assert!(serde_json::to_vec(&prepared.request)?.len() > 2 * 1024 * 1024);
    assert!(bubblepaw_core::prepared_request_bytes(&prepared.request)? < 4096);
    f.seed(&[f.message(Role::User, "active".repeat(1000))])?;
    let request = f.request(f.store.messages(&f.session)?);
    assert!(policy.estimated_tokens(&request) < 100000);
    let error = prepare_fresh(&policy, Script::normal(), &f.session, request)
        .await
        .expect_err("active byte overflow");
    assert!(
        error
            .to_string()
            .contains("prepared request exceeds the byte budget")
    );
    assert!(f.store.context_view(&f.session)?.is_none());
    Ok(())
}
#[path = "summarization/support.rs"]
mod support;
use support::new_episode_worker;
#[path = "summarization/tool_catalog.rs"]
mod tool_catalog;
#[tokio::test]
async fn failed_catchup_after_partial_success_never_publishes_a_new_view() -> Result<()> {
    let f = Fixture::new()?;
    let history = f.turns(5, 35_000);
    f.seed(&history)?;
    let (policy, _worker, models) = f.worker(60_000)?;
    models.fail_after_one.store(true, Ordering::SeqCst);
    let error = match f.prepare(&policy).await {
        Err(error) => error,
        Ok(_) => panic!("second summary fails"),
    };
    assert!(error.to_string().contains("gathering failed"));
    assert_eq!(f.store.context_episodes(&f.session)?.len(), 1);
    assert!(f.store.context_view(&f.session)?.is_none());
    assert_eq!(f.store.messages(&f.session)?, history);
    models.fail_after_one.store(false, Ordering::SeqCst);
    let prepared = f.prepare(&policy).await?;
    assert!(prepared.notice.is_some());
    let view = f.store.context_view(&f.session)?.unwrap();
    let frontier = f
        .store
        .context_view_frontier_message_id(&f.session, view.frontier)?
        .unwrap();
    let end = history.iter().position(|m| m.id == frontier).unwrap() + 1;
    assert_eq!(
        &prepared.request.messages[view.prefix.len()..],
        &history[end..]
    );
    assert_eq!(f.store.messages(&f.session)?, history);
    Ok(())
}
#[tokio::test]
async fn pressure_stops_gathering_as_soon_as_one_new_valid_episode_admits_the_request() -> Result<()>
{
    let f = Fixture::new()?;
    let history = f.turns(4, 35_000);
    f.seed(&history)?;
    let (policy, _worker, models) = f.worker(110_000)?;
    models.fail_after_one.store(true, Ordering::SeqCst);
    let prepared = f.prepare(&policy).await?;
    assert!(prepared.notice.is_some());
    assert_eq!(models.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.store.context_episodes(&f.session)?.len(), 1);
    let view = f.store.context_view(&f.session)?.unwrap();
    assert_eq!(
        &prepared.request.messages[view.prefix.len()..],
        &history[2..]
    );
    assert_eq!(f.store.messages(&f.session)?, history);
    Ok(())
}
#[tokio::test]
async fn pressure_admits_after_needed_background_episode_without_waiting_for_next_job() -> Result<()>
{
    let f = Fixture::new()?;
    let history = f.turns(4, 35_000);
    f.seed(&history)?;
    let (policy, worker, models) = f.worker(110_000)?;
    models.pause.store(true, Ordering::SeqCst);
    models.pause_on_call.store(2, Ordering::SeqCst);
    worker.trigger(&f.session, 0, 0);
    models.entered.notified().await;
    let mut preparing = tokio::spawn({
        let policy = policy.clone();
        let session = f.session.clone();
        let request = f.request(history.clone());
        async move { prepare_fresh(&*policy, Script::normal(), &session, request).await }
    });
    models.pressure_entered.notified().await;
    models.release.notify_one();
    models.entered.notified().await;
    let admitted = tokio::time::timeout(std::time::Duration::from_secs(2), &mut preparing).await;
    models.release.notify_one();
    worker.wait_idle(&f.session).await;
    let prepared = admitted
        .expect("one completed background episode must admit while the next is pending")??;
    assert!(prepared.notice.is_some());
    let view = f.store.context_view(&f.session)?.unwrap();
    assert_eq!(
        &prepared.request.messages[view.prefix.len()..],
        &history[2..]
    );
    assert_eq!(f.store.messages(&f.session)?, history);
    Ok(())
}
#[path = "summarization/chapter_compat.rs"]
mod chapter_compat;
