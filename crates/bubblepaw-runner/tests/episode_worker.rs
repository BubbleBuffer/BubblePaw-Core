#[path = "support/episode_fixture.rs"]
mod episode_fixture;
#[path = "episode_worker/events.rs"]
mod events;
use anyhow::{Result, bail};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextPreparation, Harness, HarnessConfig, Message, ModelRequest,
    ModelResponse, Provider, Role, RunOptions, Store, SubjectChapterStatus, TextCallback, ToolCall,
    ToolRegistry, Usage,
};
use bubblepaw_runner::{
    episode_models::{
        ChapterInput, ChapterResult, EpisodeModels, SplitInput, SplitResult, SummaryInput,
        SummaryResult,
    },
    episode_worker::EpisodeWorker,
    summarization::{SummarizingContext, SummaryConfig},
};
use episode_fixture::new_episode_worker;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

#[path = "common/hook.rs"]
mod hook;
use hook::{prepare_fresh, scripted_request};

struct MainProvider;

#[tokio::test]
async fn imported_compaction_advances_without_decoding_covered_tool_bodies() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_with_options(
        directory.path(),
        bubblepaw_core::StoreOptions {
            archive_after_days: 0,
            cold_min_messages: 0,
            cold_min_bytes: 0,
            cold_tail_messages: 0,
            cold_row_min_bytes: 0,
            ..Default::default()
        },
    )?;
    let session = store.create_session("imported cold continuation")?.id;
    let make = |index: usize, role: Role, content: String| Message {
        id: format!("import-continuation-{index}"),
        session_id: session.clone(),
        role,
        content,
        origin: None,
        tool_calls: Vec::new(),
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: "2026-10-12T00:00:00Z".into(),
    };
    let old_call = format!("old-{}", "identifier".repeat(80));
    let dangling = "source-call-with-unknown-outcome";
    let mut old: Vec<_> = (0..20)
        .map(|index| {
            make(
                index,
                if index == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                " old source".repeat(400),
            )
        })
        .collect();
    old[1].tool_calls.push(ToolCall {
        id: old_call.clone(),
        name: "read".into(),
        arguments: serde_json::json!({"path":"old"}),
    });
    old[2].role = Role::Tool;
    old[2].tool_call_id = Some(old_call);
    old[3].tool_calls.push(ToolCall {
        id: dangling.into(),
        name: "read".into(),
        arguments: serde_json::json!({"path":"unknown"}),
    });
    let derived = bubblepaw_core::transcript::ImportDerivedRecord {
        id: "original-source-compaction".into(),
        session_id: session.clone(),
        source_line: 30,
        source_kind: "codex_compaction".into(),
        through_message_id: Some(old.last().unwrap().id.clone()),
        content: serde_json::json!({"message":"Preserved source summary."}),
        created_at: "2026-10-12T00:00:00Z".into(),
        origin: "import".into(),
    };
    store.append_import_batch(&old, std::slice::from_ref(&derived))?;
    assert!(store.compress_cold_messages(&session)? > 0);
    let connection = rusqlite::Connection::open(store.chat_path(&session)?.unwrap())?;
    let data: String = connection.query_row(
        "SELECT data FROM messages WHERE message_id=?1",
        [&old[2].id],
        |row| row.get(0),
    )?;
    let mut data: serde_json::Value = serde_json::from_str(&data)?;
    assert_eq!(data["_row_codec"], "zstd-dict-v1");
    data["data"] = serde_json::json!("not-valid-base64!");
    connection.execute(
        "UPDATE messages SET data=?1 WHERE message_id=?2",
        rusqlite::params![data.to_string(), old[2].id],
    )?;
    drop(connection);
    assert!(store.messages(&session).is_err());
    let turns = |start: usize, count: usize| {
        (start..start + count * 2)
            .map(|index| {
                make(
                    index,
                    if index.is_multiple_of(2) {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    if index.is_multiple_of(2) {
                        format!("New request {index}.")
                    } else {
                        " datum".repeat(17_000)
                    },
                )
            })
            .collect::<Vec<_>>()
    };
    let mut fresh = turns(20, 6);
    let native_call = format!("native-{}", "identifier".repeat(80));
    fresh[1].tool_calls.push(ToolCall {
        id: native_call.clone(),
        name: "read".into(),
        arguments: serde_json::json!({"path":"native"}),
    });
    let mut native_tool = make(100, Role::Tool, " native tool output".repeat(400));
    native_tool.tool_call_id = Some(native_call);
    let native_tool_id = native_tool.id.clone();
    fresh.insert(2, native_tool);
    store.append_batch(&fresh)?;
    let policy = Arc::new(SummarizingContext::new(
        store.clone(),
        Arc::new(MainProvider),
        SummaryConfig {
            threshold_cap: 80_000,
            ..Default::default()
        },
        None,
    )?);
    let models = Arc::new(Models::default());
    let worker = new_episode_worker(store.clone(), models.clone(), policy.clone());
    policy.set_episode_worker(&worker);
    // Fresh background planning starts at source-derived coverage, preserving
    // dangling canonical call IDs without replaying or re-summarizing that past.
    worker.precompute(&session).await?;
    assert_eq!(store.context_episodes(&session)?.len(), 5);
    let first = prepare_fresh(
        &*policy,
        Arc::new(MainProvider),
        &session,
        scripted_request(store.serving_messages(&session)?),
    )
    .await?;
    assert!(first.notice.is_some());
    assert!(
        first
            .request
            .messages
            .iter()
            .any(|message| message.content == "Preserved source summary.")
    );
    let frontier = store.context_view(&session)?.unwrap().frontier.unwrap();
    // The next preparation must also skip paid native coverage, not merely the
    // older imported boundary. Poison a covered native tool's compressed body.
    assert!(store.compress_cold_messages(&session)? > 0);
    let connection = rusqlite::Connection::open(store.chat_path(&session)?.unwrap())?;
    let data: String = connection.query_row(
        "SELECT data FROM messages WHERE message_id=?1",
        [&native_tool_id],
        |row| row.get(0),
    )?;
    let mut data: serde_json::Value = serde_json::from_str(&data)?;
    assert_eq!(data["_row_codec"], "zstd-dict-v1");
    data["data"] = serde_json::json!("not-valid-base64!");
    connection.execute(
        "UPDATE messages SET data=?1 WHERE message_id=?2",
        rusqlite::params![data.to_string(), native_tool_id],
    )?;
    drop(connection);
    store.append_batch(&turns(32, 4))?;
    let tail = store
        .messages_after(&session, frontier)?
        .into_iter()
        .map(|(_, message)| message)
        .collect();
    let next = prepare_fresh(
        &*policy,
        Arc::new(MainProvider),
        &session,
        scripted_request(tail),
    )
    .await?;
    assert!(next.notice.is_some());
    assert!(store.context_view(&session)?.unwrap().frontier.unwrap() > frontier);
    assert!(
        next.request
            .messages
            .iter()
            .any(|message| message.content == "Preserved source summary.")
    );
    assert!(store.context_checkpoint(&session)?.is_none());
    assert_eq!(store.import_derived(&session)?, vec![derived]);
    let last = store.message_bounds(&session)?.1.unwrap();
    assert!(
        store
            .pending_call_ids_through(&session, last)?
            .iter()
            .any(|id| id == dangling)
    );
    Ok(())
}

#[tokio::test]
async fn completed_episode_background_planning_skips_unrelated_cold_bodies() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_with_options(
        directory.path(),
        bubblepaw_core::StoreOptions {
            archive_after_days: 0,
            cold_min_messages: 0,
            cold_min_bytes: 0,
            cold_tail_messages: 0,
            cold_row_min_bytes: 0,
            ..Default::default()
        },
    )?;
    let session = store.create_session("cold background")?.id;
    let make = |index: usize| Message {
        id: format!("background-{index}"),
        session_id: session.clone(),
        role: if index.is_multiple_of(2) {
            Role::User
        } else {
            Role::Assistant
        },
        content: if index.is_multiple_of(2) {
            format!("Original task {index}.")
        } else {
            " datum".repeat(17_000)
        },
        origin: None,
        tool_calls: Vec::new(),
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: "2026-10-12T00:00:00Z".into(),
    };
    let history: Vec<_> = (0..12).map(make).collect();
    store.append_batch(&history)?;
    let estimator = Arc::new(SummarizingContext::new(
        store.clone(),
        Arc::new(MainProvider),
        SummaryConfig::default(),
        None,
    )?);
    let models = Arc::new(Models::default());
    let worker = new_episode_worker(store.clone(), models.clone(), estimator.clone());
    worker.catch_up_until(&session, history.len()).await?;
    assert_eq!(store.context_episodes(&session)?.len(), 5);
    assert!(store.compress_cold_messages(&session)? > 0);
    let connection = rusqlite::Connection::open(store.chat_path(&session)?.unwrap())?;
    let data: String = connection.query_row(
        "SELECT data FROM messages WHERE message_id=?1",
        [&history[1].id],
        |row| row.get(0),
    )?;
    let mut data: serde_json::Value = serde_json::from_str(&data)?;
    assert_eq!(data["_row_codec"], "zstd-dict-v1");
    data["data"] = serde_json::json!("not-valid-base64!");
    connection.execute(
        "UPDATE messages SET data=?1 WHERE message_id=?2",
        rusqlite::params![data.to_string(), history[1].id],
    )?;
    drop(connection);
    assert!(store.messages(&session).is_err());
    let more: Vec<_> = (12..18).map(make).collect();
    store.append_batch(&more)?;
    // A fresh worker has no parsed-history or legacy-prefix cache to hide a read.
    let restarted = new_episode_worker(store.clone(), models.clone(), estimator);
    restarted.catch_up_until(&session, 18).await?;
    assert_eq!(store.context_episodes(&session)?.len(), 8);
    let summaries = models.summaries.lock().unwrap();
    assert_eq!(summaries.len(), 8);
    assert!(summaries[5].episode_text.contains(&history[10].content));
    assert!(summaries[5].episode_text.contains(&history[11].content));
    Ok(())
}

#[async_trait]
impl Provider for MainProvider {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: " datum".repeat(17_000),
            ..Default::default()
        })
    }
}

#[derive(Default)]
struct Models {
    splits: Mutex<Vec<SplitInput>>,
    summaries: Mutex<Vec<SummaryInput>>,
    chapters: Mutex<Vec<ChapterInput>>,
    split_once: AtomicBool,
    split_always: AtomicBool,
    split_first: AtomicBool,
    split_targets: Mutex<VecDeque<String>>,
    forced_choice: Mutex<Option<String>>,
    decision_reasoning: Mutex<Option<String>>,
    fail: AtomicBool,
    blank_summary: AtomicBool,
    protocol_summary: AtomicBool,
    blank_chapter: AtomicBool,
    fail_after_one: AtomicBool,
    revision: AtomicUsize,
    failures: Mutex<Vec<String>>,
    pause: AtomicBool,
    entered: Notify,
    release: Notify,
    active: AtomicUsize,
    peak: AtomicUsize,
    summary_repetitions: AtomicUsize,
}

#[async_trait]
impl EpisodeModels for Models {
    fn gather_revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst) as u64
    }
    fn record_drain_failure(&self, session: &str, _error: &anyhow::Error) {
        self.failures.lock().unwrap().push(session.into());
    }
    async fn decide(&self, input: SplitInput) -> Result<SplitResult> {
        let target = self.split_targets.lock().unwrap().front().cloned();
        let choice = if let Some(choice) = self.forced_choice.lock().unwrap().take() {
            Some(choice)
        } else if let Some(target) = target.filter(|target| {
            input
                .options
                .iter()
                .any(|option| &option.message_id == target)
        }) {
            self.split_targets.lock().unwrap().pop_front();
            Some(target)
        } else if self.split_first.swap(false, Ordering::SeqCst) && !input.options.is_empty() {
            Some(input.options.first().unwrap().message_id.clone())
        } else if (self.split_once.swap(false, Ordering::SeqCst)
            || self.split_always.load(Ordering::SeqCst))
            && !input.options.is_empty()
        {
            Some(input.options.last().unwrap().message_id.clone())
        } else {
            None
        };
        let mut probabilities = vec![0.0; input.options.len() + 1];
        let index = choice
            .as_ref()
            .and_then(|id| {
                input
                    .options
                    .iter()
                    .position(|option| &option.message_id == id)
            })
            .unwrap_or(input.options.len());
        probabilities[index] = 1.0;
        self.splits.lock().unwrap().push(input);
        Ok(SplitResult {
            choice,
            probabilities,
            model: "fake-decider".into(),
            usage: Usage::default(),
            reasoning: self.decision_reasoning.lock().unwrap().clone(),
        })
    }
    async fn summarize(&self, input: SummaryInput) -> Result<SummaryResult> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        let count = {
            let mut calls = self.summaries.lock().unwrap();
            calls.push(input);
            calls.len()
        };
        if self.pause.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst)
            || (self.fail_after_one.load(Ordering::SeqCst) && count > 1)
        {
            bail!("fake exhausted model retries");
        }
        Ok(SummaryResult {
            title: format!("Episode {count}"),
            summary: if self.protocol_summary.load(Ordering::SeqCst) {
                "<｜DSML｜function_calls><｜DSML｜invoke name=\"read\"/></｜DSML｜function_calls>"
                    .into()
            } else if self.blank_summary.load(Ordering::SeqCst) {
                " \n".into()
            } else {
                format!(
                    "# Episode {count}\n{}",
                    format!("Retained summary {count}.")
                        .repeat(1 + self.summary_repetitions.load(Ordering::SeqCst))
                )
            },
            model: "fake-summary".into(),
            usage: Usage::default(),
            reasoning: Some("summary reasoning".into()),
        })
    }
    async fn close_chapter(&self, input: ChapterInput) -> Result<ChapterResult> {
        self.chapters.lock().unwrap().push(input);
        Ok(ChapterResult {
            description: "Completed subject".into(),
            summary: if self.blank_chapter.load(Ordering::SeqCst) {
                " \n".into()
            } else {
                "Subject summary.".into()
            },
            related_ids: vec![],
            related_evidence: vec![],
            decisions: None,
            sections: None,
            model: "fake-chapter".into(),
            usage: Usage::default(),
            reasoning: Some("chapter reasoning".into()),
        })
    }
}

struct Fixture {
    store: Store,
    session: String,
    harness: Harness,
    estimator: Arc<SummarizingContext>,
}
impl Fixture {
    async fn new(turns: usize) -> Result<Self> {
        Self::with_provider(turns, Arc::new(MainProvider)).await
    }
    async fn with_provider(turns: usize, provider: Arc<dyn Provider>) -> Result<Self> {
        let store = Store::in_memory()?;
        let session = store.create_session("episode worker")?.id;
        let harness = Harness::new(
            store.clone(),
            provider.clone(),
            ToolRegistry::new(),
            HarnessConfig::default(),
        );
        for index in 0..turns {
            harness
                .prompt(&session, format!("request {index}"), RunOptions::default())
                .await?;
        }
        let estimator = Arc::new(SummarizingContext::new(
            store.clone(),
            provider,
            SummaryConfig::default(),
            None,
        )?);
        Ok(Self {
            store,
            session,
            harness,
            estimator,
        })
    }
    fn worker(&self, models: Arc<Models>) -> Arc<EpisodeWorker> {
        new_episode_worker(self.store.clone(), models, self.estimator.clone())
    }
}

#[tokio::test]
async fn episode_boundary_transition_closes_previous_chapter_atomically() -> Result<()> {
    let fixture = Fixture::new(4).await?;
    let models = Arc::new(Models::default());
    models.split_once.store(true, Ordering::SeqCst);
    let worker = fixture.worker(models.clone());
    let before = fixture.store.storage_stats()?.transaction_count;
    worker.precompute(&fixture.session).await?;
    let episodes = fixture.store.context_episodes(&fixture.session)?;
    let chapters = fixture.store.context_subject_chapters(&fixture.session)?;
    assert!(episodes.len() >= 2);
    assert_eq!(chapters.len(), 2);
    assert_eq!(chapters[0].status, SubjectChapterStatus::Closed);
    assert_eq!(chapters[0].last_episode_ordinal, 1);
    assert_eq!(chapters[1].first_episode_ordinal, 2);
    assert_eq!(
        fixture.store.storage_stats()?.transaction_count,
        before + episodes.len() as u64
    );
    let calls = models.splits.lock().unwrap();
    assert_eq!(calls.len(), episodes.len() - 1);
    for (call, episode) in calls.iter().zip(episodes.iter().skip(1)) {
        assert_eq!(call.options.len(), 1);
        assert_eq!(call.options[0].message_id, episode.first_message_id);
        assert!(
            call.options[0]
                .user
                .starts_with("EPISODE SUMMARY (not typed user input):")
        );
    }
    Ok(())
}

#[tokio::test]
async fn long_child_task_gathers_and_changes_phase_without_typed_input() -> Result<()> {
    use bubblepaw_core::{MessageOrigin, is_typed_user};
    use bubblepaw_runner::episodes::EpisodeConfig;
    let fixture = Fixture::new(0).await?;
    let mut anchor: Message = serde_json::from_value(serde_json::json!({
        "id":"child-anchor", "session_id":fixture.session, "role":"user",
        "content":"Implement the complete child task", "created_at":"2026-10-17T00:00:00Z"
    }))?;
    anchor.origin = Some(MessageOrigin::ChildTask);
    let mut messages = vec![anchor.clone()];
    for index in 0..4 {
        let mut message = anchor.clone();
        message.id = format!("child-round-{index}");
        message.role = Role::Assistant;
        message.origin = None;
        message.content = " datum".repeat(1600);
        messages.push(message);
    }
    fixture.store.append_batch(&messages)?;
    let models = Arc::new(Models::default());
    models.split_once.store(true, Ordering::SeqCst);
    let config = EpisodeConfig {
        summary_input: bubblepaw_runner::episodes::EpisodeSummaryInput::Episode,
        source_target_tokens: 1500,
        source_max_tokens: 2000,
        ..Default::default()
    };
    let worker = EpisodeWorker::new_with_config(
        fixture.store.clone(),
        models.clone(),
        fixture.estimator.clone(),
        config,
    )?;
    assert!(worker.trigger(&fixture.session, 0, 256_000));
    wait(&worker, &fixture.session).await?;
    let episodes = fixture.store.context_episodes(&fixture.session)?;
    assert_eq!(episodes.len(), 4);
    assert!(
        episodes
            .iter()
            .skip(1)
            .all(|e| e.origin_request_id.as_ref() == Some(&anchor.id))
    );
    assert_eq!(
        fixture
            .store
            .context_subject_chapters(&fixture.session)?
            .len(),
        2
    );
    assert_eq!(models.splits.lock().unwrap().len(), 3);
    assert!(
        models
            .summaries
            .lock()
            .unwrap()
            .iter()
            .skip(1)
            .all(|input| input
                .turn_request
                .as_ref()
                .is_some_and(|text| text.contains(&anchor.content)
                    && text.contains(&anchor.id)
                    && text.contains("ChildTask instruction")))
    );
    models.split_once.store(true, Ordering::SeqCst);
    let mut continuation = anchor.clone();
    continuation.id = "later-child-phase".into();
    continuation.role = Role::Assistant;
    continuation.origin = None;
    continuation.content = " datum".repeat(1600);
    fixture
        .store
        .append_batch(std::slice::from_ref(&continuation))?;
    worker.precompute(&fixture.session).await?;
    let chapters = models.chapters.lock().unwrap();
    assert_eq!(chapters.len(), 2);
    assert!(
        chapters
            .iter()
            .all(|input| input.typed_user_texts.is_empty()
                && input.task_instructions == vec![anchor.content.clone()])
    );
    assert!(chapters[1].episodes[0].contains(&episodes[1].first_message_id));
    drop(chapters);
    assert!(
        fixture
            .store
            .messages(&fixture.session)?
            .iter()
            .all(|m| !is_typed_user(m))
    );
    Ok(())
}

#[tokio::test]
async fn decisions_receive_bounded_accumulated_open_chapter_not_only_last_episode() -> Result<()> {
    use bubblepaw_runner::episodes::EpisodeConfig;
    let fixture = Fixture::new(5).await?;
    let models = Arc::new(Models::default());
    models.summary_repetitions.store(100, Ordering::SeqCst);
    let config = EpisodeConfig {
        summary_input: bubblepaw_runner::episodes::EpisodeSummaryInput::Episode,
        decision_context_tokens: 400,
        ..Default::default()
    };
    let worker = EpisodeWorker::new_with_config(
        fixture.store.clone(),
        models.clone(),
        fixture.estimator.clone(),
        config,
    )?;
    worker.precompute(&fixture.session).await?;
    let calls = models.splits.lock().unwrap();
    let state = &calls.last().unwrap().open_chapter;
    assert!(state.contains("Retained summary 1."));
    assert!(state.contains("Retained summary 2."));
    assert!(state.contains("Retained summary 3."));
    let episodes = fixture.store.context_episodes(&fixture.session)?;
    for episode in episodes.iter().take(3) {
        assert!(state.contains(&format!(
            "[source {}..{}]",
            episode.first_message_id, episode.last_message_id
        )));
    }
    let tokenizer = tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase)?;
    assert!(tokenizer.encode_ordinary(state).len() <= 400);
    assert_eq!(
        fixture
            .store
            .context_subject_chapters(&fixture.session)?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn same_phase_rollover_uses_accumulated_summary_tokens_without_extra_decisions() -> Result<()>
{
    use bubblepaw_runner::episodes::EpisodeConfig;
    let fixture = Fixture::new(4).await?;
    let models = Arc::new(Models::default());
    let config = EpisodeConfig {
        summary_input: bubblepaw_runner::episodes::EpisodeSummaryInput::Episode,
        chapter_rollover_tokens: 1,
        ..Default::default()
    };
    let worker = EpisodeWorker::new_with_config(
        fixture.store.clone(),
        models.clone(),
        fixture.estimator.clone(),
        config,
    )?;
    worker.precompute(&fixture.session).await?;
    let episodes = fixture.store.context_episodes(&fixture.session)?;
    let chapters = fixture.store.context_subject_chapters(&fixture.session)?;
    assert_eq!(chapters.len(), episodes.len());
    assert!(
        chapters
            .iter()
            .all(|c| c.status == SubjectChapterStatus::Closed)
    );
    assert!(chapters.iter().all(|c| {
        c.description
            .as_deref()
            .unwrap()
            .starts_with("[Continues same subject; volume rollover]")
    }));
    assert_eq!(models.chapters.lock().unwrap().len(), episodes.len());
    assert!(models.splits.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn topic_transition_and_large_current_summary_defer_second_chapter_close() -> Result<()> {
    use bubblepaw_runner::episodes::EpisodeConfig;
    let fixture = Fixture::new(1).await?;
    let models = Arc::new(Models::default());
    let config = EpisodeConfig {
        summary_input: bubblepaw_runner::episodes::EpisodeSummaryInput::Episode,
        source_target_tokens: 16_000,
        source_max_tokens: 20_000,
        chapter_rollover_tokens: 100,
        ..Default::default()
    };
    let worker = EpisodeWorker::new_with_config(
        fixture.store.clone(),
        models.clone(),
        fixture.estimator.clone(),
        config,
    )?;
    worker.precompute(&fixture.session).await?;
    models.summary_repetitions.store(100, Ordering::SeqCst);
    models.split_once.store(true, Ordering::SeqCst);
    fixture
        .harness
        .prompt(&fixture.session, "new phase", RunOptions::default())
        .await?;
    worker.precompute(&fixture.session).await?;
    let chapters = fixture.store.context_subject_chapters(&fixture.session)?;
    assert_eq!(chapters.len(), 2);
    assert_eq!(chapters[0].status, SubjectChapterStatus::Closed);
    assert_eq!(chapters[1].status, SubjectChapterStatus::Open);
    assert_eq!(fixture.store.context_episodes(&fixture.session)?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn unoffered_episode_boundary_rejects_atomic_batch_without_advancing_coverage() -> Result<()>
{
    let fixture = Fixture::new(4).await?;
    let models = Arc::new(Models::default());
    *models.forced_choice.lock().unwrap() = Some("not-offered".into());
    let worker = fixture.worker(models);
    assert!(worker.precompute(&fixture.session).await.is_err());
    let episodes = fixture.store.context_episodes(&fixture.session)?;
    assert_eq!(episodes.len(), 1);
    assert_eq!(
        fixture.store.episode_coverage_token(&fixture.session)?,
        Some(episodes[0].last_message_id.clone())
    );
    Ok(())
}

async fn wait(worker: &EpisodeWorker, session: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), worker.wait_idle(session)).await?;
    Ok(())
}

#[tokio::test]
async fn small_source_wakes_without_model_calls_and_full_source_ignores_serving_threshold()
-> Result<()> {
    let fixture = Fixture::new(0).await?;
    let models = Arc::new(Models::default());
    let worker = fixture.worker(models.clone());
    let before = fixture.store.storage_stats()?.transaction_count;
    assert!(worker.trigger(&fixture.session, 0, 256_000));
    wait(&worker, &fixture.session).await?;
    assert!(models.summaries.lock().unwrap().is_empty());
    assert_eq!(fixture.store.storage_stats()?.transaction_count, before);
    fixture
        .harness
        .prompt(&fixture.session, "one task", RunOptions::default())
        .await?;
    fixture
        .harness
        .prompt(&fixture.session, "continue", RunOptions::default())
        .await?;
    assert!(worker.trigger(&fixture.session, 1, 256_000));
    wait(&worker, &fixture.session).await?;
    assert!(!fixture.store.context_episodes(&fixture.session)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn split_atomically_closes_prefix_once_and_replans_the_remainder() -> Result<()> {
    let fixture = Fixture::new(7).await?;
    let history = fixture.store.messages(&fixture.session)?;
    let models = Arc::new(Models::default());
    models.split_once.store(true, Ordering::SeqCst);
    let worker = fixture.worker(models.clone());
    let before = fixture.store.storage_stats()?.transaction_count;
    worker.trigger(&fixture.session, 50, 100);
    wait(&worker, &fixture.session).await?;
    let episodes = fixture.store.context_episodes(&fixture.session)?;
    let chapters = fixture.store.context_subject_chapters(&fixture.session)?;
    assert!(episodes.len() >= 3);
    assert_eq!(episodes[0].last_message_id, history[1].id);
    assert_eq!(episodes[1].first_message_id, history[2].id);
    assert_eq!(chapters.len(), 2);
    assert_eq!(chapters[0].status, SubjectChapterStatus::Closed);
    assert_eq!(chapters[0].last_episode_ordinal, 1);
    assert_eq!(chapters[1].first_episode_ordinal, 2);
    assert_eq!(models.chapters.lock().unwrap().len(), 1);
    assert_eq!(
        fixture.store.storage_stats()?.transaction_count,
        before + episodes.len() as u64
    );
    assert_eq!(episodes[0].reasoning.as_deref(), Some("summary reasoning"));
    assert_eq!(chapters[0].reasoning.as_deref(), Some("chapter reasoning"));
    assert_eq!(
        models.summaries.lock().unwrap()[1]
            .previous_summary
            .as_ref(),
        Some(&episodes[0].summary)
    );
    assert_eq!(
        fixture.store.context_decisions(&fixture.session)?[0]
            .choice
            .as_ref(),
        Some(&history[2].id)
    );
    Ok(())
}

#[tokio::test]
async fn model_failure_leaves_pending_rows_and_main_run_remains_usable() -> Result<()> {
    let fixture = Fixture::new(3).await?;
    let models = Arc::new(Models::default());
    models.fail.store(true, Ordering::SeqCst);
    let worker = fixture.worker(models.clone());
    let before = fixture.store.storage_stats()?.transaction_count;
    worker.trigger(&fixture.session, 50, 100);
    wait(&worker, &fixture.session).await?;
    assert!(fixture.store.context_episodes(&fixture.session)?.is_empty());
    assert_eq!(
        *models.failures.lock().unwrap(),
        vec![fixture.session.clone()]
    );
    assert!(
        fixture
            .store
            .context_subject_chapters(&fixture.session)?
            .is_empty()
    );
    assert!(
        fixture
            .store
            .context_decisions(&fixture.session)?
            .is_empty()
    );
    assert_eq!(fixture.store.storage_stats()?.transaction_count, before);
    fixture
        .harness
        .prompt(
            &fixture.session,
            "main run still works",
            RunOptions::default(),
        )
        .await?;
    models.fail.store(false, Ordering::SeqCst);
    // New ordinary wakes cannot spend another configured attempt on the same
    // failed source. Explicit work can retry after the provider recovers.
    assert!(worker.trigger(&fixture.session, 1, 100));
    wait(&worker, &fixture.session).await?;
    assert!(fixture.store.context_episodes(&fixture.session)?.is_empty());
    assert_eq!(models.summaries.lock().unwrap().len(), 1);
    worker.precompute(&fixture.session).await?;
    assert!(!fixture.store.context_episodes(&fixture.session)?.is_empty());
    assert_eq!(models.failures.lock().unwrap().len(), 1);
    Ok(())
}

#[tokio::test]
async fn concurrent_triggers_are_single_flight_and_dirty_is_drained() -> Result<()> {
    let fixture = Fixture::new(3).await?;
    let models = Arc::new(Models::default());
    models.pause.store(true, Ordering::SeqCst);
    let worker = fixture.worker(models.clone());
    worker.trigger(&fixture.session, 50, 100);
    tokio::time::timeout(Duration::from_secs(15), models.entered.notified()).await?;
    fixture
        .harness
        .prompt(
            &fixture.session,
            "next committed turn",
            RunOptions::default(),
        )
        .await?;
    worker.trigger(&fixture.session, 70, 100);
    worker.trigger(&fixture.session, 80, 100);
    assert_eq!(models.summaries.lock().unwrap().len(), 1);
    models.release.notify_one();
    wait(&worker, &fixture.session).await?;
    assert_eq!(models.peak.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.store.context_episodes(&fixture.session)?.len(), 3);
    Ok(())
}

#[tokio::test]
async fn stale_coverage_discards_old_result_and_replans_from_saved_coverage() -> Result<()> {
    let fixture = Fixture::new(7).await?;
    let delayed = Arc::new(Models::default());
    delayed.pause.store(true, Ordering::SeqCst);
    let delayed_worker = fixture.worker(delayed.clone());
    delayed_worker.trigger(&fixture.session, 50, 100);
    tokio::time::timeout(Duration::from_secs(15), delayed.entered.notified()).await?;
    let winner = Arc::new(Models::default());
    winner.fail_after_one.store(true, Ordering::SeqCst);
    let winner_worker = fixture.worker(winner);
    let before = fixture.store.storage_stats()?.transaction_count;
    winner_worker.trigger(&fixture.session, 50, 100);
    wait(&winner_worker, &fixture.session).await?;
    assert_eq!(fixture.store.context_episodes(&fixture.session)?.len(), 1);
    delayed.release.notify_one();
    wait(&delayed_worker, &fixture.session).await?;
    let episodes = fixture.store.context_episodes(&fixture.session)?;
    assert_eq!(episodes.len(), 6);
    assert!(
        delayed.summaries.lock().unwrap()[1]
            .previous_summary
            .is_some()
    );
    assert_eq!(
        fixture.store.storage_stats()?.transaction_count,
        before + episodes.len() as u64
    );
    Ok(())
}

#[tokio::test]
async fn new_episode_boundary_closes_existing_chapter_once() -> Result<()> {
    let fixture = Fixture::new(3).await?;
    let models = Arc::new(Models::default());
    let worker = fixture.worker(models.clone());
    worker.precompute(&fixture.session).await?;
    let before_episodes = fixture.store.context_episodes(&fixture.session)?;
    let previous_end = before_episodes.last().unwrap().ordinal;
    for index in 3..5 {
        fixture
            .harness
            .prompt(
                &fixture.session,
                format!("request {index}"),
                RunOptions::default(),
            )
            .await?;
    }
    models.split_first.store(true, Ordering::SeqCst);
    let before = fixture.store.storage_stats()?.transaction_count;
    worker.precompute(&fixture.session).await?;
    let episodes = fixture.store.context_episodes(&fixture.session)?;
    let chapters = fixture.store.context_subject_chapters(&fixture.session)?;
    assert_eq!(chapters.len(), 2);
    assert_eq!(chapters[0].status, SubjectChapterStatus::Closed);
    assert_eq!(chapters[0].last_episode_ordinal, previous_end);
    assert_eq!(chapters[1].first_episode_ordinal, previous_end + 1);
    assert_eq!(models.chapters.lock().unwrap().len(), 1);
    assert_eq!(
        fixture.store.storage_stats()?.transaction_count,
        before + (episodes.len() - before_episodes.len()) as u64
    );
    Ok(())
}

#[tokio::test]
async fn abort_cancels_background_calls_and_prevents_new_wakes() -> Result<()> {
    let fixture = Fixture::new(3).await?;
    let models = Arc::new(Models::default());
    models.pause.store(true, Ordering::SeqCst);
    let worker = fixture.worker(models.clone());
    worker.trigger(&fixture.session, 50, 100);
    tokio::time::timeout(Duration::from_secs(15), models.entered.notified()).await?;
    worker.abort();
    wait(&worker, &fixture.session).await?;
    assert!(!worker.trigger(&fixture.session, 100, 100));
    assert!(fixture.store.context_episodes(&fixture.session)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn long_scripted_session_produces_many_chapters_without_changing_the_view() -> Result<()> {
    let fixture = Fixture::new(12).await?;
    let before_view = fixture.store.context_view(&fixture.session)?;
    let models = Arc::new(Models::default());
    models.split_always.store(true, Ordering::SeqCst);
    let worker = fixture.worker(models.clone());
    let before = fixture.store.storage_stats()?.transaction_count;
    worker.trigger(&fixture.session, 50, 100);
    wait(&worker, &fixture.session).await?;
    let episodes = fixture.store.context_episodes(&fixture.session)?;
    let chapters = fixture.store.context_subject_chapters(&fixture.session)?;
    assert!(episodes.len() >= 9);
    assert!(chapters.len() >= 9);
    assert!(
        chapters[..chapters.len() - 1]
            .iter()
            .all(|chapter| chapter.status == SubjectChapterStatus::Closed)
    );
    assert_eq!(chapters.last().unwrap().status, SubjectChapterStatus::Open);
    assert_eq!(
        fixture.store.storage_stats()?.transaction_count,
        before + episodes.len() as u64
    );
    assert!(
        models
            .chapters
            .lock()
            .unwrap()
            .iter()
            .any(|input| input.previous_chapters.len() == 8)
    );
    let after_view = fixture.store.context_view(&fixture.session)?;
    assert_eq!(
        serde_json::to_value(before_view)?,
        serde_json::to_value(after_view)?
    );
    Ok(())
}

#[tokio::test]
async fn explicit_lossy_summary_budget_preserves_archive_and_continuation_context() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("worker-summary-input.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("summary input projection")?.id;
    let message = |role, content| Message {
        id: uuid::Uuid::new_v4().to_string(),
        session_id: session.clone(),
        role,
        content,
        tool_calls: vec![],
        tool_call_id: None,
        origin: None,
        provider_state: None,
        is_error: false,
        created_at: "2026-10-08T00:00:00Z".into(),
    };
    let request = "literal current request\n".repeat(150);
    let arguments = serde_json::json!({"code":"serialized call argument ".repeat(200)});
    let mut history = vec![message(Role::User, request.clone())];
    for round in 0..3 {
        let call_id = format!("round-{round}");
        let mut assistant = message(Role::Assistant, format!("literal assistant round {round}"));
        assistant.tool_calls.push(ToolCall {
            id: call_id.clone(),
            name: "synthetic".into(),
            arguments: arguments.clone(),
        });
        let mut output = message(Role::Tool, serde_json::json!({"handle":format!("process:{round}"),"status":"exited","exitCode":if round == 1 { 2 } else { 0 },"stdout":format!("CANONICAL_SECRET_DATUM{}"," datum".repeat(70000)),"stderr":""}).to_string());
        output.tool_call_id = Some(call_id);
        history.extend([assistant, output]);
    }
    history.push(message(Role::User, "next typed request".into()));
    let original = serde_json::to_vec(&history)?;
    store.append_batch(&history)?;
    let estimator = Arc::new(SummarizingContext::new(
        store.clone(),
        Arc::new(MainProvider),
        SummaryConfig::default(),
        None,
    )?);
    let models = Arc::new(Models::default());
    let worker = EpisodeWorker::new_with_config(
        store.clone(),
        models.clone(),
        estimator,
        bubblepaw_runner::episodes::EpisodeConfig {
            summary_input: bubblepaw_runner::episodes::EpisodeSummaryInput::Episode,
            tool_evidence_tokens: 4_000,
            ..Default::default()
        },
    )?;
    worker.precompute(&session).await?;
    let episodes = store.context_episodes(&session)?;
    let summaries = models.summaries.lock().unwrap();
    assert_eq!(summaries.len(), 3);
    for (round, input) in summaries.iter().enumerate() {
        let output = &history[2 + 2 * round];
        let chars = output.content.chars().count();
        assert!(input.episode_text.contains(&format!(
            "[tool output round-{round}, {chars} chars, {}]",
            if round == 1 { "error" } else { "ok" }
        )));
        assert!(input.episode_text.contains("CANONICAL_SECRET_DATUM"));
        assert!(input.episode_text.contains(&arguments.to_string()));
        assert!(input.episode_text.contains("excerpt clipped"));
        assert!(
            input
                .episode_text
                .contains(&format!("literal assistant round {round}"))
        );
        if round == 0 {
            assert!(input.episode_text.contains(&request));
            assert!(input.previous_summary.is_none());
            assert!(input.turn_request.is_none());
        } else {
            assert_eq!(
                input.previous_summary.as_ref(),
                Some(&episodes[round - 1].summary)
            );
            assert!(
                input
                    .turn_request
                    .as_ref()
                    .is_some_and(|text| text.contains(&request)
                        && text.contains(&history[0].id)
                        && text.contains("Typed request"))
            );
        }
    }
    assert!(models.chapters.lock().unwrap().is_empty());
    assert_eq!(serde_json::to_vec(&store.messages(&session)?)?, original);
    Ok(())
}

// These tests exercise gathering/apply with deterministic episode-only models.
// Fork transport is covered separately by captured-request adapter tests.
#[path = "episode_worker/chapter_compat.rs"]
mod chapter_compat;

#[path = "episode_worker/retry.rs"]
mod retry;
