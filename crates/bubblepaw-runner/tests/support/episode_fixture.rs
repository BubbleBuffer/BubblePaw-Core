//! Episode worker builder shared by runner integration tests.

pub fn new_episode_worker(
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
