#[path = "support/episode_fixture.rs"]
mod episode_fixture;
use anyhow::{Result, bail};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextStrategy, Harness, HarnessConfig, Message, ModelRequest,
    ModelResponse, Provider, Role, RunOptions, Store, TextCallback, ToolRegistry, Usage,
};
use bubblepaw_runner::{
    context_strategies::compact_command,
    episode_models::{
        ChapterInput, ChapterResult, EpisodeModels, SplitInput, SplitResult, SummaryInput,
        SummaryResult,
    },
    episode_worker::EpisodeWorker,
    summarization::{SummarizingContext, SummaryConfig},
};
use episode_fixture::new_episode_worker;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;

#[path = "common/hook.rs"]
mod hook;
use hook::{prepare_call, prepare_fresh};

#[path = "manual_compaction/apply_facts.rs"]
mod apply_facts;
#[path = "manual_compaction/transactions.rs"]
mod transactions;

struct NoMain;
#[async_trait]
impl Provider for NoMain {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        bail!("manual compaction must not invoke the main model")
    }
}
#[derive(Default)]
struct Models {
    inputs: Mutex<Vec<SummaryInput>>,
    fail: AtomicBool,
    pause: AtomicBool,
    entered: Notify,
    release: Notify,
}
#[async_trait]
impl EpisodeModels for Models {
    async fn decide(&self, input: SplitInput) -> Result<SplitResult> {
        let mut probabilities = vec![0.0; input.options.len() + 1];
        *probabilities.last_mut().unwrap() = 1.0;
        Ok(SplitResult {
            choice: None,
            probabilities,
            model: "scripted".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
    async fn summarize(&self, input: SummaryInput) -> Result<SummaryResult> {
        self.inputs.lock().unwrap().push(input);
        if self.pause.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        if self.fail.load(Ordering::SeqCst) {
            bail!("scripted summary failure");
        }
        Ok(SummaryResult {
            title: "Retained facts".into(),
            summary: "# Retained facts\nOriginal decisions retained.".into(),
            model: "scripted".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
    async fn close_chapter(&self, _: ChapterInput) -> Result<ChapterResult> {
        bail!("no subject changes in this fixture")
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    session: String,
    models: Arc<Models>,
    context: Arc<SummarizingContext>,
    worker: Arc<EpisodeWorker>,
    harness: Harness,
}
impl Fixture {
    fn new(strategy: ContextStrategy) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let store = Store::open_data_dir(dir.path())?;
        let session = store
            .create_session_with_strategy("Manual compact", strategy)?
            .id;
        let context = Arc::new(
            SummarizingContext::new(
                store.clone(),
                Arc::new(NoMain),
                SummaryConfig::default(),
                None,
            )?
            .with_auto_summarize(false),
        );
        let models = Arc::new(Models::default());
        let worker = new_episode_worker(store.clone(), models.clone(), context.clone());
        context.set_episode_worker(&worker);
        let harness = Harness::new(
            store.clone(),
            Arc::new(NoMain),
            ToolRegistry::default(),
            HarnessConfig {
                model: "scripted".into(),
                system_prompt: "Keep original instructions".into(),
                context: context.clone(),
                ..Default::default()
            },
        );
        Ok(Self {
            _dir: dir,
            store,
            session,
            models,
            context,
            worker,
            harness,
        })
    }
    fn message(&self, role: Role, content: impl Into<String>) -> Message {
        Message {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: self.session.clone(),
            role,
            origin: None,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: "2026-10-15T00:00:00Z".into(),
        }
    }
    fn turns(&self, count: usize) -> Result<()> {
        for index in 0..count {
            self.store.append_batch(&[
                self.message(Role::User, format!("Exact request {index}")),
                self.message(Role::Assistant, format!("Evidence for {index}")),
            ])?;
        }
        Ok(())
    }
}

#[test]
fn slash_compact_accepts_optional_focus_only_at_command_boundary() {
    assert_eq!(compact_command("/compact"), Some(None));
    assert_eq!(
        compact_command(" /compact   preserve exact paths  "),
        Some(Some("preserve exact paths"))
    );
    assert_eq!(
        compact_command("/compact\nkeep decisions"),
        Some(Some("keep decisions"))
    );
    assert_eq!(compact_command("/compacted"), None);
    assert_eq!(compact_command("say /compact"), None);
}

#[tokio::test]
async fn manual_focus_does_not_leak_into_background_work() -> Result<()> {
    let f = Fixture::new(ContextStrategy::Lcm)?;
    f.turns(2)?;
    let original = f.store.messages(&f.session)?;
    f.harness
        .compact_session(&f.session, Some("Keep paths"))
        .await?;
    let large_round = f.message(Role::Assistant, " datum".repeat(34_000));
    let next_task = f.message(Role::User, "Next task");
    f.store
        .append_batch(&[large_round.clone(), next_task.clone()])?;
    f.worker.precompute(&f.session).await?;
    let episodes = f.store.context_episodes(&f.session)?;
    assert_eq!(
        episodes
            .iter()
            .map(|episode| (&episode.first_message_id, &episode.last_message_id))
            .collect::<Vec<_>>(),
        [
            (&original[0].id, &original[1].id),
            (&original[2].id, &original[3].id),
            (&large_round.id, &large_round.id),
        ]
    );
    assert_eq!(
        f.store.episode_coverage_token(&f.session)?.as_deref(),
        Some(large_round.id.as_str())
    );
    assert_eq!(
        f.store.messages(&f.session)?.last().unwrap().id,
        next_task.id
    );
    let inputs = f.models.inputs.lock().unwrap();
    // The >32k indivisible round stands alone after the prior complete round.
    assert_eq!(inputs.len(), 3);
    assert_eq!(inputs[0].focus.as_deref(), Some("Keep paths"));
    assert!(inputs[1..].iter().all(|input| input.focus.is_none()));
    Ok(())
}

#[tokio::test]
async fn failed_manual_catchup_preserves_previous_view_and_history() -> Result<()> {
    let f = Fixture::new(ContextStrategy::Lcm)?;
    f.turns(2)?;
    f.harness.compact_session(&f.session, None).await?;
    let previous = serde_json::to_value(f.store.context_view(&f.session)?)?;
    f.turns(2)?;
    let history = f.store.messages(&f.session)?;
    f.models.fail.store(true, Ordering::SeqCst);
    assert!(
        f.harness
            .compact_session(&f.session, Some("focus"))
            .await
            .is_err()
    );
    assert_eq!(
        serde_json::to_value(f.store.context_view(&f.session)?)?,
        previous
    );
    assert_eq!(f.store.messages(&f.session)?, history);
    // The failed future releases session maintenance ownership.
    f.models.fail.store(false, Ordering::SeqCst);
    f.harness.compact_session(&f.session, None).await?;
    Ok(())
}

#[tokio::test]
async fn compaction_reserves_session_and_cancels_without_publishing_a_view() -> Result<()> {
    let f = Fixture::new(ContextStrategy::Lcm)?;
    f.turns(2)?;
    let history = f.store.messages(&f.session)?;
    f.models.pause.store(true, Ordering::SeqCst);
    let compact = f.harness.compact_session(&f.session, None);
    tokio::pin!(compact);
    tokio::select! {
        result = &mut compact => panic!("compaction finished before pause: {result:?}"),
        _ = f.models.entered.notified() => {}
    }
    assert!(
        f.harness
            .start(&f.session, "racing input", RunOptions::default())
            .unwrap_err()
            .to_string()
            .contains("compacting")
    );
    assert!(f.store.delete_session(&f.session).is_err());
    assert!(f.store.fork(&f.session, "fork").is_err());
    assert!(f.harness.cancel(&f.session));
    assert!(compact.await.is_err());
    assert!(f.store.context_view(&f.session)?.is_none());
    assert_eq!(f.store.messages(&f.session)?, history);
    f.harness.compact_session(&f.session, None).await?;
    Ok(())
}

#[tokio::test]
async fn active_run_rejects_manual_compaction() -> Result<()> {
    let f = Fixture::new(ContextStrategy::Lcm)?;
    f.harness
        .start(&f.session, "active turn", RunOptions::default())?;
    let error = f
        .harness
        .compact_session(&f.session, None)
        .await
        .unwrap_err();
    assert!(
        error
            .downcast_ref::<bubblepaw_core::SessionBusy>()
            .is_some()
    );
    f.harness.wait_for_idle(&f.session).await?;
    assert!(f.models.inputs.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn disabled_auto_preserves_exact_request_until_explicit_compaction_with_runtime_notes()
-> Result<()> {
    let f = Fixture::new(ContextStrategy::Lcm)?;
    f.turns(2)?;
    let artifacts = Arc::new(bubblepaw_runner::artifacts::ArtifactStore::new(
        f._dir.path(),
        f._dir.path().join("artifacts"),
    )?);
    let repl = Arc::new(bubblepaw_runner::codemode::CodeMode::new(
        f._dir.path().to_path_buf(),
        artifacts,
    )?);
    let notes = Arc::new(bubblepaw_runner::runtime_note::RuntimeNotes::without_processes(repl));
    let context = Arc::new(
        SummarizingContext::new(
            f.store.clone(),
            Arc::new(NoMain),
            SummaryConfig::default(),
            Some(notes),
        )?
        .with_auto_summarize(false),
    );
    let worker = new_episode_worker(f.store.clone(), f.models.clone(), context.clone());
    context.set_episode_worker(&worker);
    let request = ModelRequest {
        model: "scripted".into(),
        system_prompt: "Keep original instructions".into(),
        messages: f.store.messages(&f.session)?,
        ..Default::default()
    };
    let before = serde_json::to_value(&request)?;
    let prepared = prepare_fresh(&*context, Arc::new(NoMain), &f.session, request.clone()).await?;
    assert_eq!(serde_json::to_value(prepared.request)?, before);
    assert!(prepared.notice.is_none());
    assert!(f.models.inputs.lock().unwrap().is_empty());
    prepare_call(
        &*context,
        Arc::new(NoMain),
        &bubblepaw_core::ContextState::default(),
        &f.session,
        request.clone(),
        bubblepaw_core::PrepareMode::Compact { focus: None },
        CancellationToken::new(),
    )
    .await?;
    assert!(f.store.context_view(&f.session)?.is_some());
    let state = bubblepaw_core::ContextState::default();
    let prepared = prepare_call(
        &*context,
        Arc::new(NoMain),
        &state,
        &f.session,
        request,
        bubblepaw_core::PrepareMode::Request,
        CancellationToken::new(),
    )
    .await?;
    assert!(
        prepared
            .request
            .messages
            .iter()
            .any(|message| message.content.contains("Original decisions retained."))
    );
    // The runtime facts are the harness's runtime_inventory reminder, not part of the view.
    assert!(prepared.request.messages.iter().all(|message| {
        !message
            .content
            .starts_with("[host notice]\nCurrent volatile runtime facts")
    }));
    // The view's lookup hint is staged as a cold note: it renders in the cold tier.
    let indexes = bubblepaw_core::ContextPreparation::cold_message_indexes(
        &*context,
        &f.session,
        &prepared.request,
    )?;
    let mut placed = prepared.request.clone();
    state.place_cold_notes(&mut placed, &f.session, "root", &indexes);
    assert_eq!(
        placed.messages.len(),
        prepared.request.messages.len() + 1,
        "the view's lookup hint is a cold note"
    );
    assert!(
        placed
            .messages
            .iter()
            .any(|message| message.content.contains("history_lookup")),
    );
    assert_eq!(f.models.inputs.lock().unwrap().len(), 1);
    Ok(())
}

// These tests exercise gathering/apply with deterministic episode-only models.
// Fork transport is covered separately by captured-request adapter tests.
