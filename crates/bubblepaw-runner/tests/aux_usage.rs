use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    Answer, AnswerKind, CancellationToken, DecisionProvider, DecisionRequest, DecisionResponse,
    DecisionUsage, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, ProviderError,
    ProviderErrorKind, RequestKind, RunOptions, Store, TextCallback, ToolRegistry, Usage,
};
use bubblepaw_runner::{
    episode_models::{
        ChapterInput, ClassicalInput, EpisodeModelConfig, EpisodeModels, HttpEpisodeModels,
        SplitInput, SplitOption, SummaryInput,
    },
    model_roles::{ModelRole, ModelRoles},
    provider::{ProviderRegistry, RetryPolicy},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Fixture {
    decisions: AtomicUsize,
    fail_first_decision: bool,
}
fn usage() -> Usage {
    Usage {
        input_tokens: 19,
        output_tokens: 7,
        cache_read_tokens: 11,
        cache_creation_tokens: 3,
        reasoning_tokens: Some(2),
    }
}
#[async_trait]
impl Provider for Fixture {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: "# Fixture\nA useful factual summary.".into(),
            usage: Some(usage()),
            ..Default::default()
        })
    }
}
#[async_trait]
impl DecisionProvider for Fixture {
    async fn decide(
        &self,
        request: DecisionRequest,
        _: CancellationToken,
    ) -> Result<DecisionResponse> {
        if self.decisions.fetch_add(1, Ordering::SeqCst) == 0 && self.fail_first_decision {
            let mut error =
                ProviderError::new(ProviderErrorKind::Transient, "billable fixture error");
            error.usage = Some(usage());
            return Err(error.into());
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
            usage: DecisionUsage::from(usage()),
        })
    }
}
fn registry(store: &Store, fail_first_decision: bool) -> Result<Arc<ProviderRegistry>> {
    let fixture = Arc::new(Fixture {
        decisions: AtomicUsize::new(0),
        fail_first_decision,
    });
    let mut registry = ProviderRegistry::new("fixture", fixture.clone());
    registry.insert_decision("decision", fixture)?;
    registry.set_usage_store(store.clone());
    Ok(Arc::new(registry))
}
fn roles() -> ModelRoles {
    let mut roles = ModelRoles::for_models("fixture/model".into(), "fixture/model".into());
    roles.decider.models = vec!["decision/sample".into()];
    roles.decider.repeats = Some(1);
    roles
}
#[tokio::test]
async fn main_summary_and_decider_each_retain_one_request() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("request usage fixture")?.id;
    let registry = registry(&store, false)?;
    Harness::new(
        store.clone(),
        registry.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            model: "fixture/model".into(),
            ..Default::default()
        },
    )
    .prompt(&session, "retain this fact", RunOptions::default())
    .await?;
    let models = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles: roles(),
            retry_base_delay: Duration::ZERO,
        },
        registry,
        None,
    )?;
    models
        .summarize(SummaryInput {
            focus: None,
            session_id: session.clone(),
            previous_summary: None,
            turn_request: None,
            episode_text: "retained fact".into(),
            cap: 100,
        })
        .await?;
    models
        .decide(SplitInput {
            session_id: session.clone(),
            open_chapter: "prior".into(),
            options: vec![SplitOption {
                message_id: "first".into(),
                user: "new subject".into(),
                agent_before: String::new(),
            }],
        })
        .await?;
    let records = store.request_usage()?;
    assert_eq!(records.len(), 3);
    for kind in [
        RequestKind::Main,
        RequestKind::SummaryFork,
        RequestKind::Decider,
    ] {
        let row = records.iter().find(|row| row.kind == kind).unwrap();
        assert_eq!(row.session_id, session);
        let expected = if kind == RequestKind::Decider {
            // Custom decision adapters expose the existing DecisionUsage type,
            // which has no reasoning count. Native adapters capture raw usage.
            Usage {
                reasoning_tokens: None,
                ..usage()
            }
        } else {
            usage()
        };
        assert_eq!(row.usage, expected);
        assert!(!row.failed);
        assert!(uuid::Uuid::parse_str(&row.request_id).is_ok());
        assert!(!row.created_at.is_empty());
        assert_eq!(row.run_id.is_some(), kind == RequestKind::Main);
        assert_eq!(
            row.provider,
            if kind == RequestKind::Decider {
                "decision"
            } else {
                "fixture"
            }
        );
        assert_eq!(
            row.model,
            if kind == RequestKind::Decider {
                "sample"
            } else {
                "model"
            }
        );
    }
    assert_ne!(records[0].request_id, records[1].request_id);
    assert_ne!(records[1].request_id, records[2].request_id);
    Ok(())
}

#[tokio::test]
async fn repeated_decision_samples_and_failed_billable_retry_are_separate_rows() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("decision retries")?.id;
    let registry = registry(&store, true)?;
    let role = ModelRole {
        repeats: Some(2),
        ..roles().decider
    };
    let response = role
        .decide_with_retry(
            &registry,
            DecisionRequest {
                session_id: Some(session.clone()),
                ..Default::default()
            },
            CancellationToken::new(),
            Duration::ZERO,
            Arc::new(|_, _, _| {}),
        )
        .await?;
    let records = store.request_usage()?;
    assert_eq!(records.len(), 3);
    assert_eq!(records.iter().filter(|row| row.failed).count(), 1);
    assert_eq!(response.usage.input_tokens, 3 * usage().input_tokens);
    for row in records {
        assert_eq!(row.kind, RequestKind::Decider);
        assert_eq!(row.session_id, session);
        assert_eq!((&*row.provider, &*row.model), ("decision", "sample"));
        let expected = if row.failed {
            usage()
        } else {
            Usage {
                reasoning_tokens: None,
                ..usage()
            }
        };
        assert_eq!(row.usage, expected);
    }
    Ok(())
}

#[tokio::test]
async fn classical_and_chapter_requests_keep_existing_summary_kind() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("other auxiliary roles")?.id;
    let models = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles: roles(),
            retry_base_delay: Duration::ZERO,
        },
        registry(&store, false)?,
        None,
    )?;
    models
        .summarize_classical(ClassicalInput {
            session_id: session.clone(),
            previous_summary: None,
            latest_user_request: None,
            span_text: "facts".into(),
        })
        .await?;
    models
        .close_chapter(ChapterInput {
            session_id: session.clone(),
            episodes: vec!["facts".into()],
            previous_chapters: vec![],
            typed_user_texts: vec![],
            task_instructions: vec![],
        })
        .await?;
    let records = store.request_usage()?;
    assert_eq!(records.len(), 2);
    assert!(
        records
            .iter()
            .all(|row| row.kind == RequestKind::SummaryFork
                && row.session_id == session
                && row.provider == "fixture"
                && row.model == "model"
                && row.usage == usage()
                && !row.failed)
    );
    Ok(())
}

struct FailedProvider;
#[async_trait]
impl Provider for FailedProvider {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let mut error = ProviderError::new(ProviderErrorKind::Transient, "billable failed route");
        error.usage = Some(usage());
        Err(error.into())
    }
}
#[tokio::test]
async fn fallback_routes_keep_each_actual_provider_and_usage() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("fallback billing")?.id;
    let mut registry = ProviderRegistry::new("failed", Arc::new(FailedProvider));
    registry.insert(
        "fallback",
        Arc::new(Fixture {
            decisions: AtomicUsize::new(0),
            fail_first_decision: false,
        }),
    )?;
    registry.set_model_fallbacks("failed/first".into(), vec!["fallback/second".into()])?;
    registry.set_retry_policy(RetryPolicy::once());
    registry.set_usage_store(store.clone());
    registry
        .generate(
            ModelRequest {
                session_id: session.clone(),
                model: "failed/first".into(),
                ..Default::default()
            },
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await?;
    let records = store.request_usage()?;
    assert_eq!(records.len(), 2);
    let failed = records.iter().find(|row| row.failed).unwrap();
    assert_eq!((&*failed.provider, &*failed.model), ("failed", "first"));
    let completed = records.iter().find(|row| !row.failed).unwrap();
    assert_eq!(
        (&*completed.provider, &*completed.model),
        ("fallback", "second")
    );
    assert!(
        records
            .iter()
            .all(|row| row.session_id == session && row.usage == usage())
    );
    Ok(())
}
