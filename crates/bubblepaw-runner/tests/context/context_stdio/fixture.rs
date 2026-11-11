use super::*;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextStrategy, Harness, HarnessConfig, ModelRequest, ModelResponse,
    Provider, Role, RunOptions, TextCallback, ToolRegistry, Usage,
};
use bubblepaw_runner::{
    episode_models::{
        ChapterInput, ChapterResult, EpisodeModels, SplitInput, SplitResult, SummaryInput,
        SummaryResult,
    },
    summarization::{SummarizingContext, SummaryConfig},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Main(AtomicUsize);
#[async_trait]
impl Provider for Main {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> anyhow::Result<ModelResponse> {
        let index = self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ModelResponse {
            text: format!("Original response {index}: {}", " datum".repeat(1_000)),
            usage: Some(Usage {
                input_tokens: 100,
                output_tokens: if index == 1 { 0 } else { 42 },
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}
struct Models;
#[async_trait]
impl EpisodeModels for Models {
    async fn decide(&self, input: SplitInput) -> anyhow::Result<SplitResult> {
        let mut probabilities = vec![0.0; input.options.len() + 1];
        probabilities[0] = 1.0;
        Ok(SplitResult {
            choice: Some(input.options[0].message_id.clone()),
            probabilities,
            model: "scripted".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
    async fn summarize(&self, _: SummaryInput) -> anyhow::Result<SummaryResult> {
        Ok(SummaryResult {
            title: "Retained evidence".into(),
            summary: "Original decisions retained in complete episode memory.".into(),
            model: "scripted".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
    async fn close_chapter(&self, _: ChapterInput) -> anyhow::Result<ChapterResult> {
        Ok(ChapterResult {
            description: "First phase".into(),
            summary: "Closed original decisions retained.".into(),
            related_ids: vec![],
            related_evidence: vec![],
            decisions: None,
            sections: None,
            model: "scripted".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
}

pub struct Fixture {
    pub root: tempfile::TempDir,
    pub store: Store,
    pub session: String,
    pub history: Vec<bubblepaw_core::Message>,
    pub seq: Vec<i64>,
    pub episode: bubblepaw_core::ContextEpisode,
    pub chapter: bubblepaw_core::ContextSubjectChapter,
    pub closed_chapter: bubblepaw_core::ContextSubjectChapter,
    pub view: bubblepaw_core::ContextView,
}
impl Fixture {
    pub async fn new() -> anyhow::Result<Self> {
        let root = tempfile::tempdir()?;
        let store = Store::open_data_dir(root.path().join("data"))?;
        let session = store
            .create_session_with_strategy_and_workspace(
                "Context reads",
                ContextStrategy::Lcm,
                root.path(),
            )?
            .id;
        let provider = Arc::new(Main(AtomicUsize::new(0)));
        let context = Arc::new(
            SummarizingContext::new(
                store.clone(),
                provider.clone(),
                SummaryConfig::default(),
                None,
            )?
            .with_auto_summarize(false),
        );
        let worker = bubblepaw_runner::episode_worker::EpisodeWorker::new_with_config(
            store.clone(),
            Arc::new(Models),
            context.clone(),
            bubblepaw_runner::episodes::EpisodeConfig {
                summary_input: bubblepaw_runner::episodes::EpisodeSummaryInput::Episode,
                source_target_tokens: 500,
                source_max_tokens: 1_500,
                ..Default::default()
            },
        )?;
        context.set_episode_worker(&worker);
        let harness = Harness::new(
            store.clone(),
            provider,
            ToolRegistry::default(),
            HarnessConfig {
                model: "scripted".into(),
                system_prompt: "Preserve the exact originals".into(),
                context,
                ..Default::default()
            },
        );
        for index in 0..3 {
            harness
                .prompt(
                    &session,
                    format!("Original request {index}"),
                    RunOptions::default(),
                )
                .await?;
        }
        let history = store.messages(&session)?;
        assert_eq!(history.len(), 6);
        harness.compact_session(&session, None).await?;
        assert_eq!(store.messages(&session)?, history);
        let seq = history
            .iter()
            .map(|message| store.message_seq(&session, &message.id))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let episode = store
            .context_episodes(&session)?
            .into_iter()
            .next()
            .unwrap();
        let chapters = store.context_subject_chapters(&session)?;
        let chapter = chapters
            .iter()
            .find(|chapter| chapter.status == bubblepaw_core::SubjectChapterStatus::Open)
            .unwrap()
            .clone();
        let closed_chapter = chapters
            .into_iter()
            .find(|chapter| chapter.status == bubblepaw_core::SubjectChapterStatus::Closed)
            .unwrap();
        let view = store.context_view(&session)?.unwrap();
        assert!(view.blocks_estimated);
        store.flush_serving()?;
        assert_eq!(history[1].role, Role::Assistant);
        Ok(Self {
            root,
            store,
            session,
            history,
            seq,
            episode,
            chapter,
            closed_chapter,
            view,
        })
    }
}
