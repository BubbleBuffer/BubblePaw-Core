use super::*;
use bubblepaw_runner::episodes::{EpisodeConfig, EpisodeSummaryInput};

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

/// A provider that cannot fork summaries (the trait default), as every non-chat adapter.
struct NoFork;
#[async_trait]
impl Provider for NoFork {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse::default())
    }
}

async fn gather_with(
    provider: Arc<dyn Provider>,
    summary_input: EpisodeSummaryInput,
) -> Result<(Arc<ForkModels>, Result<()>)> {
    let f = Fixture::new()?;
    let source = vec![
        f.message(Role::User, "task"),
        f.message(Role::Assistant, "complete exact source"),
    ];
    f.store.append_batch(&source)?;
    let request = f.request(source.clone());
    f.context.observe_request(&f.session, &request, provider);
    f.context
        .observe_response(&f.session, &ModelResponse::default());
    let models = Arc::new(ForkModels::default());
    let worker = EpisodeWorker::new_with_config(
        f.store.clone(),
        models.clone(),
        f.context.clone(),
        EpisodeConfig {
            summary_input,
            ..Default::default()
        },
    )?;
    let result = worker.compact_until(&f.session, source.len(), None).await;
    Ok((models, result))
}

#[test]
fn default_summary_input_is_auto() {
    assert_eq!(
        EpisodeConfig::default().summary_input,
        EpisodeSummaryInput::Auto
    );
}

#[tokio::test]
async fn auto_forks_when_the_provider_supports_it() -> Result<()> {
    let (models, result) =
        gather_with(Arc::new(Recorded::default()), EpisodeSummaryInput::Auto).await?;
    result?;
    assert_eq!(models.forks.lock().unwrap().len(), 1);
    assert_eq!(*models.bounded.lock().unwrap(), 0);
    Ok(())
}

#[tokio::test]
async fn auto_summarizes_from_episode_input_when_the_provider_cannot_fork() -> Result<()> {
    let (models, result) = gather_with(Arc::new(NoFork), EpisodeSummaryInput::Auto).await?;
    result?;
    assert!(models.forks.lock().unwrap().is_empty());
    assert_eq!(*models.bounded.lock().unwrap(), 1);
    Ok(())
}

#[tokio::test]
async fn explicit_fork_still_selects_the_fork_path_on_an_unsupported_provider() -> Result<()> {
    // The strict error comes from the summary call (see
    // unsupported_fork_requires_explicit_episode_policy); the worker must not swap the input.
    let (models, result) = gather_with(Arc::new(NoFork), EpisodeSummaryInput::Fork).await?;
    result?;
    assert_eq!(models.forks.lock().unwrap().len(), 1);
    assert_eq!(*models.bounded.lock().unwrap(), 0);
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
