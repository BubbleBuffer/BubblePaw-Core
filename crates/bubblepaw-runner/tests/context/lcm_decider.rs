//! Scripted provider dispatch through the real config, model roles and gather worker.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    Answer, AnswerKind, CancellationToken, DecisionProvider, DecisionRequest, DecisionResponse,
    DecisionUsage, Message, ModelRequest, ModelResponse, Provider, ProviderError,
    ProviderErrorKind, RequestKind, Role, Store, SubjectChapterStatus, TextCallback, Usage,
};
use bubblepaw_runner::{
    config::{EffectiveConfig, LoadOptions, Override},
    episode_models::{EpisodeModelConfig, HttpEpisodeModels},
    episode_worker::EpisodeWorker,
    provider::{ProviderRegistry, RetryPolicy},
    summarization::{SummarizingContext, SummaryConfig},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

const SUMMARY: &str = "# Retained work\nThe validated source evidence is preserved.";

#[derive(Default)]
struct CountingProvider {
    summaries: AtomicUsize,
    chapters: AtomicUsize,
    decisions: AtomicUsize,
    fail_decisions: AtomicBool,
}
#[async_trait]
impl Provider for CountingProvider {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let text = match request.model.as_str() {
            "summary" => {
                self.summaries.fetch_add(1, Ordering::SeqCst);
                SUMMARY
            }
            "chapter" => {
                self.chapters.fetch_add(1, Ordering::SeqCst);
                "# Completed work\nThe validated result remains available.\nRelated: none"
            }
            model => anyhow::bail!("unexpected scripted model {model}"),
        };
        Ok(ModelResponse {
            text: text.into(),
            usage: Some(Usage {
                input_tokens: 30,
                output_tokens: 15,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}
#[async_trait]
impl DecisionProvider for CountingProvider {
    async fn decide(
        &self,
        request: DecisionRequest,
        _: CancellationToken,
    ) -> Result<DecisionResponse> {
        self.decisions.fetch_add(1, Ordering::SeqCst);
        if self.fail_decisions.load(Ordering::SeqCst) {
            return Err(ProviderError::new(
                ProviderErrorKind::Fatal,
                "scripted decider unavailable",
            )
            .into());
        }
        Ok(DecisionResponse {
            model: request.model,
            answers: vec![Answer {
                name: "choice".into(),
                kind: AnswerKind::Choice {
                    choice: "none".into(),
                    probabilities: vec![("1".into(), 0.0), ("none".into(), 1.0)],
                },
            }],
            usage: DecisionUsage::default(),
        })
    }
}

fn options(workspace: &std::path::Path) -> LoadOptions {
    LoadOptions {
        workspace: workspace.into(),
        home: None,
        custom_file: None,
        env: vec![],
        cli: vec![],
    }
}

#[test]
fn decider_switch_defaults_on_and_layers_as_a_boolean() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut opts = options(root.path());
    assert!(
        EffectiveConfig::load(opts.clone())?
            .settings()?
            .lcm
            .episodes
            .decider_enabled
    );
    opts.env = vec![(
        "BUBBLEPAW_CONTEXT__LCM__DECIDER_ENABLED".into(),
        "false".into(),
    )];
    let disabled = EffectiveConfig::load(opts.clone())?;
    assert!(!disabled.settings()?.lcm.episodes.decider_enabled);
    let shown: toml::Value = toml::from_str(&disabled.render()?)?;
    assert_eq!(
        shown["context"]["lcm"]["decider_enabled"].as_bool(),
        Some(false)
    );
    opts.cli = vec![Override::set("context.lcm.decider_enabled=true")?];
    assert!(
        EffectiveConfig::load(opts.clone())?
            .settings()?
            .lcm
            .episodes
            .decider_enabled
    );
    opts.cli = vec![Override::set("context.lcm.decider_enabled='off'")?];
    assert!(EffectiveConfig::load(opts).is_err());
    Ok(())
}

struct Fixture {
    store: Store,
    session: String,
    source: Vec<Message>,
    provider: Arc<CountingProvider>,
    worker: Arc<EpisodeWorker>,
}
impl Fixture {
    fn new(overrides: &[String]) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let mut opts = options(root.path());
        for entry in [
            "context.lcm.summary_input='episode'",
            "context.lcm.source_target_tokens=100",
            "context.lcm.source_max_tokens=200",
            "roles.summary=['fixture/summary']",
            "roles.chapter=['fixture/chapter']",
            "roles.decider={models=['decision/choice'],repeats=1}",
        ] {
            opts.cli.push(Override::set(entry)?);
        }
        for entry in overrides {
            opts.cli.push(Override::set(entry)?);
        }
        let settings = EffectiveConfig::load(opts)?.settings()?;
        let store = Store::in_memory()?;
        let session = store.create_session("decider switch")?.id;
        let source = (0..8)
            .map(|index| Message {
                id: format!("source-{index}"),
                session_id: session.clone(),
                role: if index % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                content: if index % 2 == 0 {
                    format!("Task {index}")
                } else {
                    " datum".repeat(300)
                },
                origin: None,
                tool_calls: vec![],
                tool_call_id: None,
                provider_state: None,
                is_error: false,
                created_at: "2026-10-23T00:00:00Z".into(),
            })
            .collect::<Vec<_>>();
        store.append_batch(&source)?;
        let provider = Arc::new(CountingProvider::default());
        let mut registry = ProviderRegistry::new("fixture", provider.clone());
        registry.insert_decision("decision", provider.clone())?;
        registry.set_retry_policy(RetryPolicy::once());
        registry.set_usage_store(store.clone());
        let registry = Arc::new(registry);
        let models = HttpEpisodeModels::from_registry(
            EpisodeModelConfig {
                roles: settings.roles,
                retry_base_delay: Duration::ZERO,
            },
            registry.clone(),
            None,
        )?
        .with_episode_config(settings.lcm.episodes.clone())?;
        let estimator = Arc::new(SummarizingContext::new(
            store.clone(),
            registry,
            SummaryConfig::default(),
            None,
        )?);
        let worker = EpisodeWorker::new_with_config(
            store.clone(),
            Arc::new(models),
            estimator,
            settings.lcm.episodes,
        )?;
        Ok(Self {
            store,
            session,
            source,
            provider,
            worker,
        })
    }
}

#[tokio::test]
async fn disabled_decider_finishes_summaries_without_decision_calls_or_records() -> Result<()> {
    let fixture = Fixture::new(&["context.lcm.decider_enabled=false".into()])?;
    fixture
        .provider
        .fail_decisions
        .store(true, Ordering::SeqCst);
    fixture.worker.precompute(&fixture.session).await?;
    assert_eq!(fixture.provider.summaries.load(Ordering::SeqCst), 4);
    assert_eq!(fixture.provider.decisions.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.provider.chapters.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.store.context_episodes(&fixture.session)?.len(), 4);
    assert!(
        fixture
            .store
            .context_decisions(&fixture.session)?
            .is_empty()
    );
    assert_eq!(
        fixture.store.episode_coverage_token(&fixture.session)?,
        Some(fixture.source[7].id.clone())
    );
    let chapters = fixture.store.context_subject_chapters(&fixture.session)?;
    assert_eq!(chapters.len(), 1);
    assert_eq!(chapters[0].status, SubjectChapterStatus::Open);
    assert!(
        fixture
            .store
            .request_usage()?
            .iter()
            .all(|row| row.kind != RequestKind::Decider)
    );
    Ok(())
}

#[tokio::test]
async fn disabled_decider_rolls_over_by_accumulated_summary_tokens() -> Result<()> {
    let tokenizer = tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase)?;
    let rollover = tokenizer.encode_ordinary(SUMMARY).len() * 2;
    let fixture = Fixture::new(&[
        "context.lcm.decider_enabled=false".into(),
        format!("context.lcm.chapter_rollover_tokens={rollover}"),
    ])?;
    fixture
        .provider
        .fail_decisions
        .store(true, Ordering::SeqCst);
    fixture.worker.precompute(&fixture.session).await?;
    assert_eq!(fixture.provider.summaries.load(Ordering::SeqCst), 4);
    assert_eq!(fixture.provider.decisions.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.provider.chapters.load(Ordering::SeqCst), 2);
    assert!(
        fixture
            .store
            .context_decisions(&fixture.session)?
            .is_empty()
    );
    let chapters = fixture.store.context_subject_chapters(&fixture.session)?;
    assert_eq!(chapters.len(), 2);
    assert_eq!(
        (
            chapters[0].first_episode_ordinal,
            chapters[0].last_episode_ordinal
        ),
        (1, 2)
    );
    assert_eq!(
        (
            chapters[1].first_episode_ordinal,
            chapters[1].last_episode_ordinal
        ),
        (3, 4)
    );
    assert!(
        chapters
            .iter()
            .all(|chapter| chapter.status == SubjectChapterStatus::Closed)
    );
    Ok(())
}

#[tokio::test]
async fn enabled_decider_failure_is_fatal_without_advancing_failed_episode() -> Result<()> {
    let fixture = Fixture::new(&[])?;
    fixture
        .provider
        .fail_decisions
        .store(true, Ordering::SeqCst);
    let error = fixture
        .worker
        .precompute(&fixture.session)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("scripted decider unavailable"));
    assert_eq!(fixture.provider.summaries.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.provider.decisions.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.provider.chapters.load(Ordering::SeqCst), 0);
    let episodes = fixture.store.context_episodes(&fixture.session)?;
    assert_eq!(episodes.len(), 1);
    assert_eq!(
        fixture.store.episode_coverage_token(&fixture.session)?,
        Some(fixture.source[1].id.clone())
    );
    assert!(
        fixture
            .store
            .context_decisions(&fixture.session)?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn enabled_decider_keeps_default_semantic_calls() -> Result<()> {
    let fixture = Fixture::new(&[])?;
    fixture.worker.precompute(&fixture.session).await?;
    assert_eq!(fixture.provider.summaries.load(Ordering::SeqCst), 4);
    assert_eq!(fixture.provider.decisions.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.store.context_decisions(&fixture.session)?.len(), 3);
    Ok(())
}
