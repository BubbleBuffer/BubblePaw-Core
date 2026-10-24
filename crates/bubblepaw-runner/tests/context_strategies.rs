#[path = "support/episode_fixture.rs"]
mod episode_fixture;
use anyhow::{Result, bail};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextStrategy, Message, ModelRequest, ModelResponse, Provider, Role,
    Store, TextCallback, ToolCall, Usage,
};
use bubblepaw_runner::{
    context_strategies::{TYPED_USER_TOKENS, files_touched, typed_user_block},
    episode_models::{
        ChapterInput, ChapterResult, ClassicalInput, ClassicalResult, EpisodeModels, SplitInput,
        SplitResult, SummaryInput, SummaryResult,
    },
    episode_worker::EpisodeWorker,
    summarization::{SummarizingContext, SummaryConfig},
};
use episode_fixture::new_episode_worker;
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tiktoken_rs::{bpe_for_tokenizer, tokenizer::Tokenizer};
use tokio::sync::Notify;
#[path = "common/hook.rs"]
mod hook;
use hook::{prepare_call, prepare_fresh, scripted_request};

struct NoProvider;
#[async_trait]
impl Provider for NoProvider {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        bail!("context preparation must not invoke the main model")
    }
}
#[derive(Default)]
struct Models {
    inputs: Mutex<Vec<ClassicalInput>>,
    lcm_calls: AtomicUsize,
    pause_call: AtomicUsize,
    entered: Notify,
    release: Notify,
}
#[async_trait]
impl EpisodeModels for Models {
    async fn decide(&self, _: SplitInput) -> Result<SplitResult> {
        self.lcm_calls.fetch_add(1, Ordering::SeqCst);
        bail!("unexpected decision")
    }
    async fn summarize(&self, _: SummaryInput) -> Result<SummaryResult> {
        self.lcm_calls.fetch_add(1, Ordering::SeqCst);
        bail!("unexpected episode summary")
    }
    async fn close_chapter(&self, _: ChapterInput) -> Result<ChapterResult> {
        self.lcm_calls.fetch_add(1, Ordering::SeqCst);
        bail!("unexpected chapter")
    }
    async fn summarize_classical(&self, input: ClassicalInput) -> Result<ClassicalResult> {
        let count = {
            let mut inputs = self.inputs.lock().unwrap();
            inputs.push(input);
            inputs.len()
        };
        if self.pause_call.load(Ordering::SeqCst) == count {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(ClassicalResult {
            summary: format!("anchored summary {count}"),
            model: "scripted-summary".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    session: String,
    policy: Arc<SummarizingContext>,
    worker: Arc<EpisodeWorker>,
    models: Arc<Models>,
}
impl Fixture {
    fn new(strategy: ContextStrategy, threshold: usize) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("archive.sqlite");
        let store = Store::open(&path)?;
        let session = store
            .create_session_with_strategy("strategy test", strategy)?
            .id;
        let policy = Arc::new(SummarizingContext::new(
            store.clone(),
            Arc::new(NoProvider),
            SummaryConfig {
                threshold_cap: threshold,
                ..Default::default()
            },
            None,
        )?);
        let models = Arc::new(Models::default());
        let worker = new_episode_worker(store.clone(), models.clone(), policy.clone());
        policy.set_episode_worker(&worker);
        Ok(Self {
            _dir: dir,
            store,
            session,
            policy,
            worker,
            models,
        })
    }
    fn message(&self, role: Role, content: impl Into<String>) -> Message {
        Message {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: self.session.clone(),
            role,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
            origin: None,
            provider_state: None,
            is_error: false,
            created_at: "2026-10-09T12:00:00Z".into(),
        }
    }
    fn seed(&self, messages: &[Message]) -> Result<()> {
        self.store.append_batch(messages)
    }
    fn turns(&self, count: usize) -> Vec<Message> {
        (0..count)
            .flat_map(|index| {
                [
                    self.message(Role::User, format!("literal request {index}")),
                    self.message(
                        Role::Assistant,
                        format!("assistant-sentinel-{index}{}", " datum".repeat(36_000)),
                    ),
                ]
            })
            .collect()
    }
    async fn prepare(&self) -> Result<bubblepaw_core::PreparedContext> {
        prepare_fresh(
            &*self.policy,
            Arc::new(NoProvider),
            &self.session,
            scripted_request(self.store.messages(&self.session)?),
        )
        .await
    }
}

#[tokio::test]
async fn classical_rebuilds_compacted_view_after_serving_wipe_without_a_worker() -> Result<()> {
    let fixture = Fixture::new(ContextStrategy::Classical, 80_000)?;
    fixture.seed(&fixture.turns(3))?;
    let first = fixture.prepare().await?;
    assert!(first.notice.is_some());
    let calls = fixture.models.inputs.lock().unwrap().len();
    assert_eq!(calls, 1);
    let history = serde_json::to_vec(&fixture.store.messages(&fixture.session)?)?;
    let checkpoint = fixture.store.context_checkpoint(&fixture.session)?.unwrap();
    assert!(fixture.store.context_view(&fixture.session)?.is_some());
    fixture.store.serving_wipe()?;
    assert!(fixture.store.context_view(&fixture.session)?.is_none());
    let restarted = SummarizingContext::new(
        fixture.store.clone(),
        Arc::new(NoProvider),
        SummaryConfig {
            threshold_cap: 80_000,
            ..Default::default()
        },
        None,
    )?;
    let request = || scripted_request(fixture.store.messages(&fixture.session).unwrap());
    let recovered = prepare_fresh(
        &restarted,
        Arc::new(NoProvider),
        &fixture.session,
        request(),
    )
    .await?;
    assert!(restarted.estimated_tokens(&recovered.request) < 80_000);
    assert_eq!(fixture.models.inputs.lock().unwrap().len(), calls);
    assert_eq!(fixture.models.lcm_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        serde_json::to_vec(&fixture.store.messages(&fixture.session)?)?,
        history
    );
    let after = fixture.store.context_checkpoint(&fixture.session)?.unwrap();
    assert_eq!(
        (
            &checkpoint.id,
            &checkpoint.summary,
            &checkpoint.through_message_id
        ),
        (&after.id, &after.summary, &after.through_message_id)
    );
    let content = |request: &ModelRequest| {
        request.messages.iter().map(|message| json!({"role":message.role,"content":message.content,"tool_calls":message.tool_calls,"tool_call_id":message.tool_call_id,"provider_state":message.provider_state,"is_error":message.is_error})).collect::<Vec<_>>()
    };
    assert_eq!(content(&first.request), content(&recovered.request));
    assert_eq!(first.request.system_prompt, recovered.request.system_prompt);
    let repeated = prepare_fresh(
        &restarted,
        Arc::new(NoProvider),
        &fixture.session,
        request(),
    )
    .await?;
    assert_eq!(
        serde_json::to_vec(&repeated.request)?,
        serde_json::to_vec(&recovered.request)?
    );
    Ok(())
}

#[tokio::test]
async fn classical_updates_previous_plus_new_span_and_merges_recorded_files() -> Result<()> {
    let fixture = Fixture::new(ContextStrategy::Classical, 80_000)?;
    let mut first = fixture.turns(3);
    first[1].tool_calls.push(ToolCall {
        id: "read-call".into(),
        name: "read".into(),
        arguments: json!({"path":"src/old.rs"}),
    });
    let mut output = fixture.message(Role::Tool, "tool observation ".repeat(200));
    output.tool_call_id = Some("read-call".into());
    first.insert(2, output.clone());
    fixture.seed(&first)?;
    fixture
        .worker
        .catch_up_until(&fixture.session, first.len())
        .await?;
    let previous = fixture.store.context_checkpoint(&fixture.session)?.unwrap();
    assert_eq!(previous.files_touched, ["src/old.rs"]);
    assert!(
        fixture.models.inputs.lock().unwrap()[0]
            .span_text
            .contains("chars, ok]")
    );
    assert!(
        fixture.models.inputs.lock().unwrap()[0]
            .span_text
            .contains(&output.content)
    );
    let mut next = fixture.turns(1);
    next[0].content = "new request exactly".into();
    next[0].tool_calls.push(ToolCall {
        id: "edit-call".into(),
        name: "tools.write".into(),
        arguments: json!({"path":"src/new.rs","content":"new file\n"}),
    });
    let mut edited = fixture.message(Role::Tool, "edited");
    edited.tool_call_id = Some("edit-call".into());
    next.insert(1, edited);
    fixture.seed(&next)?;
    fixture
        .worker
        .catch_up_until(&fixture.session, first.len() + next.len())
        .await?;
    let latest = fixture.store.context_checkpoint(&fixture.session)?.unwrap();
    assert_eq!(latest.files_touched, ["src/new.rs", "src/old.rs"]);
    let inputs = fixture.models.inputs.lock().unwrap();
    assert_eq!(inputs.len(), 2);
    assert_eq!(
        inputs[1].previous_summary.as_deref(),
        Some(previous.summary.as_str())
    );
    assert_eq!(
        inputs[1].latest_user_request.as_deref(),
        Some("new request exactly")
    );
    assert!(!inputs[1].span_text.contains("assistant-sentinel-1"));
    assert!(!inputs[1].span_text.contains("src/old.rs"));
    assert!(inputs[1].span_text.contains("src/new.rs"));
    assert_eq!(fixture.models.lcm_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture.store.messages(&fixture.session)?.len(),
        first.len() + next.len()
    );
    Ok(())
}

#[tokio::test]
async fn classical_precompute_stays_invisible_then_uses_ready_checkpoint() -> Result<()> {
    let fixture = Fixture::new(ContextStrategy::Classical, 100_000)?;
    let first = fixture.turns(2);
    fixture.seed(&first)?;
    assert!(fixture.worker.trigger(&fixture.session, 72_000, 100_000));
    fixture.worker.wait_idle(&fixture.session).await;
    assert!(
        fixture
            .store
            .context_checkpoint(&fixture.session)?
            .is_some()
    );
    assert!(fixture.store.context_view(&fixture.session)?.is_none());
    let prepared = fixture.prepare().await?;
    assert!(prepared.notice.is_none());
    assert_eq!(
        serde_json::to_value(&prepared.request.messages)?,
        serde_json::to_value(&first)?
    );
    fixture.seed(&fixture.turns(1))?;
    let prepared = fixture.prepare().await?;
    assert!(prepared.notice.is_some());
    assert_eq!(fixture.models.inputs.lock().unwrap().len(), 1);
    assert!(
        prepared
            .request
            .messages
            .iter()
            .any(|message| message.content.contains("anchored summary 1"))
    );
    let typed = prepared
        .request
        .messages
        .iter()
        .find(|message| message.content.starts_with("Typed user messages ("))
        .unwrap();
    assert!(typed.content.contains(&first[0].content));
    assert!(fixture.store.context_view(&fixture.session)?.is_some());
    Ok(())
}

#[tokio::test]
async fn classical_missing_ready_checkpoint_blocks_for_one_summary() -> Result<()> {
    let fixture = Fixture::new(ContextStrategy::Classical, 80_000)?;
    fixture.seed(&fixture.turns(3))?;
    let prepared = fixture.prepare().await?;
    assert!(prepared.notice.is_some());
    assert_eq!(fixture.models.inputs.lock().unwrap().len(), 1);
    assert!(fixture.policy.estimated_tokens(&prepared.request) < 80_000);
    assert_eq!(fixture.models.lcm_calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn missing_ready_checkpoint_waits_until_summary_is_released() -> Result<()> {
    let fixture = Fixture::new(ContextStrategy::Classical, 80_000)?;
    fixture.seed(&fixture.turns(3))?;
    fixture.models.pause_call.store(1, Ordering::SeqCst);
    let preparing = fixture.prepare();
    tokio::pin!(preparing);
    tokio::select! {
        result = &mut preparing => panic!("Compaction finished before the missing summary was ready: {result:?}"),
        _ = fixture.models.entered.notified() => {}
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut preparing)
            .await
            .is_err()
    );
    assert!(fixture.store.context_view(&fixture.session)?.is_none());
    fixture.models.release.notify_one();
    let prepared = preparing.await?;
    assert!(prepared.notice.is_some());
    assert_eq!(fixture.models.inputs.lock().unwrap().len(), 1);
    Ok(())
}

#[tokio::test]
async fn cancelling_missing_ready_refresh_preserves_the_previous_frozen_view() -> Result<()> {
    let fixture = Fixture::new(ContextStrategy::Classical, 80_000)?;
    fixture.seed(&fixture.turns(3))?;
    fixture.prepare().await?;
    let frozen = serde_json::to_value(fixture.store.context_view(&fixture.session)?)?;
    let checkpoint = serde_json::to_value(fixture.store.context_checkpoint(&fixture.session)?)?;
    fixture.seed(&fixture.turns(3))?;
    let history = fixture.store.messages(&fixture.session)?;
    fixture.models.pause_call.store(2, Ordering::SeqCst);
    let cancellation = CancellationToken::new();
    let state = bubblepaw_core::ContextState::default();
    let preparing = prepare_call(
        &*fixture.policy,
        Arc::new(NoProvider),
        &state,
        &fixture.session,
        scripted_request(history.clone()),
        bubblepaw_core::PrepareMode::Request,
        cancellation.clone(),
    );
    tokio::pin!(preparing);
    tokio::select! {
        result = &mut preparing => panic!("Refresh finished before cancellation: {result:?}"),
        _ = fixture.models.entered.notified() => {}
    }
    cancellation.cancel();
    assert!(
        preparing
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    assert_eq!(
        serde_json::to_value(fixture.store.context_view(&fixture.session)?)?,
        frozen
    );
    assert_eq!(
        serde_json::to_value(fixture.store.context_checkpoint(&fixture.session)?)?,
        checkpoint
    );
    assert_eq!(
        serde_json::to_value(fixture.store.messages(&fixture.session)?)?,
        serde_json::to_value(history)?
    );
    Ok(())
}

#[tokio::test]
async fn ready_checkpoint_does_not_wait_for_inflight_background_refresh() -> Result<()> {
    let fixture = Fixture::new(ContextStrategy::Classical, 100_000)?;
    fixture.seed(&fixture.turns(2))?;
    fixture
        .worker
        .catch_up_until(&fixture.session, usize::MAX)
        .await?;
    fixture.seed(&fixture.turns(1))?;
    fixture.models.pause_call.store(2, Ordering::SeqCst);
    // An overlapping explicit refresh can hold the gate while this request's
    // snapshot already fits the earlier ready checkpoint. Ordinary precompute
    // correctly leaves an unused pending checkpoint alone.
    let worker = fixture.worker.clone();
    let session = fixture.session.clone();
    let refresh = tokio::spawn(async move { worker.catch_up_until(&session, usize::MAX).await });
    fixture.models.entered.notified().await;
    let prepared =
        tokio::time::timeout(std::time::Duration::from_secs(1), fixture.prepare()).await??;
    assert!(
        prepared
            .request
            .messages
            .iter()
            .any(|message| message.content.contains("anchored summary 1"))
    );
    let frozen = serde_json::to_value(fixture.store.context_view(&fixture.session)?)?;
    fixture.models.release.notify_one();
    refresh.await??;
    assert_eq!(
        fixture
            .store
            .context_checkpoint(&fixture.session)?
            .unwrap()
            .summary,
        "anchored summary 2"
    );
    assert_eq!(
        serde_json::to_value(fixture.store.context_view(&fixture.session)?)?,
        frozen
    );
    let repeated = fixture.prepare().await?;
    assert!(repeated.notice.is_none());
    assert!(
        repeated
            .request
            .messages
            .iter()
            .any(|message| message.content.contains("anchored summary 1"))
    );
    Ok(())
}

#[tokio::test]
async fn classical_keeps_one_unused_precomputation_until_the_checkpoint_is_applied() -> Result<()> {
    let fixture = Fixture::new(ContextStrategy::Classical, 100_000)?;
    fixture.seed(&fixture.turns(2))?;
    assert!(fixture.worker.trigger(&fixture.session, 72_000, 100_000));
    fixture.worker.wait_idle(&fixture.session).await;
    let first = fixture.store.context_checkpoint(&fixture.session)?.unwrap();
    let mut additional = fixture.turns(1);
    additional.extend((0..10).map(|index| {
        fixture.message(
            Role::Assistant,
            format!("new complete message {index}{}", " datum".repeat(100)),
        )
    }));
    for message in additional {
        fixture.seed(&[message])?;
        let before = fixture.store.storage_stats()?.transaction_count;
        fixture.worker.trigger(&fixture.session, 109_000, 100_000);
        fixture.worker.wait_idle(&fixture.session).await;
        fixture.worker.precompute(&fixture.session).await?;
        assert_eq!(fixture.store.storage_stats()?.transaction_count, before);
    }
    assert_eq!(fixture.models.inputs.lock().unwrap().len(), 1);
    assert_eq!(
        fixture
            .store
            .context_checkpoint(&fixture.session)?
            .unwrap()
            .id,
        first.id
    );
    assert!(fixture.store.context_view(&fixture.session)?.is_none());
    let prepared = fixture.prepare().await?;
    assert!(prepared.notice.is_some());
    assert_eq!(fixture.models.inputs.lock().unwrap().len(), 1);
    fixture.worker.precompute(&fixture.session).await?;
    let pending = fixture.store.context_checkpoint(&fixture.session)?.unwrap();
    assert_ne!(pending.id, first.id);
    assert_eq!(fixture.models.inputs.lock().unwrap().len(), 2);
    fixture.seed(&fixture.turns(1))?;
    let writes = fixture.store.storage_stats()?.transaction_count;
    for _ in 0..3 {
        fixture.worker.trigger(&fixture.session, 110_000, 100_000);
        fixture.worker.wait_idle(&fixture.session).await;
        fixture.worker.precompute(&fixture.session).await?;
    }
    assert_eq!(fixture.models.inputs.lock().unwrap().len(), 2);
    assert_eq!(
        fixture
            .store
            .context_checkpoint(&fixture.session)?
            .unwrap()
            .id,
        pending.id
    );
    assert_eq!(fixture.store.storage_stats()?.transaction_count, writes);
    Ok(())
}

#[tokio::test]
async fn classical_force_refreshes_a_pending_checkpoint_that_no_longer_fits() -> Result<()> {
    let fixture = Fixture::new(ContextStrategy::Classical, 100_000)?;
    fixture.seed(&fixture.turns(2))?;
    fixture.worker.precompute(&fixture.session).await?;
    let pending = fixture.store.context_checkpoint(&fixture.session)?.unwrap();
    fixture.seed(&fixture.turns(3))?;
    fixture.worker.precompute(&fixture.session).await?;
    assert_eq!(fixture.models.inputs.lock().unwrap().len(), 1);
    assert_eq!(
        fixture
            .store
            .context_checkpoint(&fixture.session)?
            .unwrap()
            .id,
        pending.id
    );
    let prepared = fixture.prepare().await?;
    assert!(prepared.notice.is_some());
    assert!(fixture.policy.estimated_tokens(&prepared.request) < 100_000);
    assert_eq!(fixture.models.inputs.lock().unwrap().len(), 2);
    assert_ne!(
        fixture
            .store
            .context_checkpoint(&fixture.session)?
            .unwrap()
            .id,
        pending.id
    );
    assert_eq!(fixture.models.lcm_calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn classical_coalesced_wakes_do_not_rewrite_the_newly_pending_checkpoint() -> Result<()> {
    let fixture = Fixture::new(ContextStrategy::Classical, 100_000)?;
    fixture.seed(&fixture.turns(2))?;
    let before = fixture.store.storage_stats()?.chat_transaction_count;
    fixture.models.pause_call.store(1, Ordering::SeqCst);
    fixture.worker.trigger(&fixture.session, 72_000, 100_000);
    fixture.models.entered.notified().await;
    fixture.seed(&fixture.turns(1))?;
    for _ in 0..5 {
        fixture.worker.trigger(&fixture.session, 108_000, 100_000);
    }
    fixture.models.release.notify_one();
    fixture.worker.wait_idle(&fixture.session).await;
    assert_eq!(fixture.models.inputs.lock().unwrap().len(), 1);
    assert!(
        fixture
            .store
            .context_checkpoint(&fixture.session)?
            .is_some()
    );
    assert!(fixture.store.context_view(&fixture.session)?.is_none());
    // The concurrent canonical append and one checkpoint each commit once;
    // disposable serving telemetry has its own independent transactions.
    assert_eq!(
        fixture.store.storage_stats()?.chat_transaction_count,
        before + 2
    );
    Ok(())
}

#[tokio::test]
async fn classical_missing_summary_timeout_leaves_the_frozen_view_and_archive_unchanged()
-> Result<()> {
    let mut fixture = Fixture::new(ContextStrategy::Classical, 80_000)?;
    fixture.policy = Arc::new(SummarizingContext::new(
        fixture.store.clone(),
        Arc::new(NoProvider),
        SummaryConfig {
            threshold_cap: 80_000,
            timeout: std::time::Duration::from_millis(20),
            ..Default::default()
        },
        None,
    )?);
    fixture.worker = new_episode_worker(
        fixture.store.clone(),
        fixture.models.clone(),
        fixture.policy.clone(),
    );
    fixture.policy.set_episode_worker(&fixture.worker);
    fixture.models.pause_call.store(1, Ordering::SeqCst);
    let history = fixture.turns(3);
    fixture.seed(&history)?;
    let error = fixture.prepare().await.unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert!(
        fixture
            .store
            .context_checkpoint(&fixture.session)?
            .is_none()
    );
    assert!(fixture.store.context_view(&fixture.session)?.is_none());
    assert_eq!(
        serde_json::to_value(fixture.store.messages(&fixture.session)?)?,
        serde_json::to_value(history)?
    );
    Ok(())
}

#[test]
fn typed_users_are_newest_first_whole_and_production_token_bounded_with_all_older_refs()
-> Result<()> {
    let fixture = Fixture::new(ContextStrategy::Classical, 80_000)?;
    let tokenizer = bpe_for_tokenizer(Tokenizer::O200kBase)?;
    let mut users = (0..5)
        .map(|index| {
            fixture.message(
                Role::User,
                format!("request-{index}{}", " datum".repeat(6_000)),
            )
        })
        .collect::<Vec<_>>();
    let mut synthetic = fixture.message(Role::User, "not typed, must be excluded");
    synthetic.origin = Some(bubblepaw_core::MessageOrigin::Generated);
    users.push(synthetic);
    let block = typed_user_block(&users, tokenizer);
    assert!(tokenizer.encode_ordinary(&block).len() <= TYPED_USER_TOKENS);
    assert!(block.contains(&users[4].content));
    assert!(block.contains(&users[3].content));
    assert!(block.contains(&users[2].content));
    assert!(!block.contains(&users[1].content));
    for message in &users[..2] {
        assert!(block.contains(&message.id));
    }
    assert!(block.find("request-4").unwrap() < block.find("request-3").unwrap());
    assert!(!block.contains("not typed"));
    Ok(())
}

#[test]
fn file_list_uses_recorded_paths_and_nested_arguments_without_prose_inference() -> Result<()> {
    let fixture = Fixture::new(ContextStrategy::Classical, 80_000)?;
    let mut message = fixture.message(Role::Assistant, "prose mentions invented.rs");
    message.tool_calls = vec![
        ToolCall {
            id: "read".into(),
            name: "read".into(),
            arguments: json!({"path":"z.rs"}),
        },
        ToolCall {
            id: "write".into(),
            name: "tools.write".into(),
            arguments: json!({"path":"a.rs"}),
        },
        ToolCall {
            id: "nested".into(),
            name: "batch".into(),
            arguments: json!({"calls":[
                {"name":"edit","arguments":{"path":"nested.rs"}},
                {"name":"tools.write","arguments":{"path":"new.rs","content":"prose mentions unparsed.rs"}},
                {"name":"tools.edit","arguments":{"path":"source.rs","old_string":"before","new_string":"after"}},
                {"name":"unsupported","arguments":{"path":"unparsed.rs"}}
            ]}),
        },
        ToolCall {
            id: "js".into(),
            name: "js".into(),
            arguments: json!({"code":"tools.read({path: 'opaque.rs'})"}),
        },
        ToolCall {
            id: "search".into(),
            name: "search".into(),
            arguments: json!({"path":"directory","edits":[{"path":"not-an-edit.rs"}]}),
        },
    ];
    assert_eq!(
        files_touched(&["earlier.rs".into(), "a.rs".into()], &[message]),
        [
            "a.rs",
            "earlier.rs",
            "nested.rs",
            "new.rs",
            "source.rs",
            "z.rs"
        ]
    );
    Ok(())
}

// These tests exercise gathering/apply with deterministic episode-only models.
// Fork transport is covered separately by captured-request adapter tests.
