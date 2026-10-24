// These tests exercise gathering/apply with deterministic episode-only models.
// Fork transport is covered separately by captured-request adapter tests.
pub(super) fn new_episode_worker(
    store: bubblepaw_core::Store,
    models: std::sync::Arc<dyn bubblepaw_runner::episode_models::EpisodeModels>,
    estimator: std::sync::Arc<bubblepaw_runner::summarization::SummarizingContext>,
) -> std::sync::Arc<bubblepaw_runner::episode_worker::EpisodeWorker> {
    bubblepaw_runner::episode_worker::EpisodeWorker::new_with_config(
        store,
        models,
        estimator,
        bubblepaw_runner::episodes::EpisodeConfig {
            summary_input: bubblepaw_runner::episodes::EpisodeSummaryInput::Episode,
            ..Default::default()
        },
    )
    .unwrap()
}
