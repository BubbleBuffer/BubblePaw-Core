//! Auxiliary assembled requests share the core context boundary, including non-wire providers.
#[path = "support/chat_messages.rs"]
mod chat_messages;
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ModelRequest, ModelResponse, Provider, TextCallback, Usage,
};
use bubblepaw_runner::{
    episode_models::{
        ChapterInput, ClassicalInput, EpisodeFork, EpisodeModelConfig, EpisodeModels,
        HttpEpisodeModels, SplitInput, SplitOption, SummaryInput,
    },
    model_roles::ModelRoles,
    provider::ProviderRegistry,
    trace::{JsonlTrace, TraceConfig},
};
use chat_messages::root_message;
use serde_json::Value;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Capture(Mutex<Vec<ModelRequest>>);
#[async_trait]
impl Provider for Capture {
    fn supports_summary_fork(&self, _: &str) -> bool {
        true
    }
    fn summary_fork_mapping(&self, request: &ModelRequest) -> Result<Option<Value>> {
        Ok(Some(serde_json::json!({"sources":request.messages.iter()
            .map(|message| serde_json::json!({"source_id":message.id})).collect::<Vec<_>>()})))
    }
    fn estimate_summary_fork_tokens(
        &self,
        _: &ModelRequest,
        _: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        Ok(Some(10))
    }
    async fn generate_summary_fork(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        self.generate(request, Arc::new(|_| {}), cancellation).await
    }
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let decision = request.system_prompt.contains("Allowed options:");
        self.0.lock().unwrap().push(request);
        Ok(ModelResponse {
            text: if decision {
                r#"{"choice":"none"}"#
            } else {
                "# Recorded memory\nThe source evidence remains unchanged.\nRelated: none"
            }
            .into(),
            usage: Some(Usage {
                input_tokens: 12,
                output_tokens: 4,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}
fn summary(session: &str) -> SummaryInput {
    SummaryInput {
        session_id: session.into(),
        focus: None,
        previous_summary: None,
        turn_request: None,
        episode_text: "Canonical evidence".into(),
        cap: 2000,
    }
}

#[tokio::test]
async fn auxiliary_dispatch_builds_once_per_call_with_continuous_session_lineage() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("trace.jsonl");
    let trace = JsonlTrace::open(TraceConfig {
        path: path.clone(),
        ..Default::default()
    })?;
    let provider = Arc::new(Capture::default());
    let registry = Arc::new(ProviderRegistry::new("script", provider.clone()));
    let mut roles = ModelRoles::for_models("script/model".into(), String::new());
    roles.decider.models = vec!["llm/script/model".into()];
    roles.decider.repeats = Some(1);
    let models = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles,
            retry_base_delay: Duration::ZERO,
        },
        registry,
        Some(trace.clone()),
    )?;
    models.summarize(summary("root")).await?;
    models.summarize(summary("root")).await?;
    models.summarize(summary("child")).await?;
    models
        .summarize_classical(ClassicalInput {
            session_id: "root".into(),
            previous_summary: None,
            latest_user_request: None,
            span_text: "new span".into(),
        })
        .await?;
    models
        .close_chapter(ChapterInput {
            session_id: "root".into(),
            episodes: vec!["memory".into()],
            previous_chapters: vec![],
            typed_user_texts: vec![],
            task_instructions: vec![],
        })
        .await?;
    models
        .decide(SplitInput {
            session_id: "root".into(),
            open_chapter: "state".into(),
            options: vec![SplitOption {
                message_id: "m1".into(),
                user: "task".into(),
                agent_before: "progress".into(),
            }],
        })
        .await?;
    trace.flush(Duration::from_secs(3))?;
    let status = trace.status();
    assert_eq!(status.dropped, 0);
    assert_eq!(status.write_errors, 0);
    let rows: Vec<Value> = std::fs::read_to_string(path)?
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).map(|envelope| envelope["record"].clone()))
        .collect::<std::result::Result<_, _>>()?;
    let built: Vec<_> = rows
        .iter()
        .filter(|row| row["kind"] == "context_built")
        .collect();
    assert_eq!(provider.0.lock().unwrap().len(), 6);
    assert_eq!(
        built.len(),
        6,
        "decorators and observational wire-shape rebuilds must not build twice"
    );
    let kinds: Vec<_> = built
        .iter()
        .map(|row| row["metrics"]["request_kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "summary",
            "summary",
            "summary",
            "classical",
            "chapter",
            "decider:choice"
        ]
    );
    assert_eq!(built[0]["metrics"]["classification"]["kind"], "first");
    assert_eq!(built[1]["metrics"]["classification"]["kind"], "append");
    assert_eq!(built[2]["metrics"]["classification"]["kind"], "first");
    assert_eq!(built[2]["session_id"], "child");
    assert!(built.iter().all(|row| row.get("payload").is_none()));
    assert_eq!(
        rows.iter()
            .filter(|row| row["metrics"]["event"] == "episode_model_call")
            .count(),
        6
    );
    Ok(())
}

#[tokio::test]
async fn fork_carries_exact_cold_annotations_and_preserves_captured_prefix() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("trace.jsonl");
    let trace = JsonlTrace::open(TraceConfig {
        path: path.clone(),
        ..Default::default()
    })?;
    let provider = Arc::new(Capture::default());
    let models = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles: ModelRoles::for_models("script/model".into(), String::new()),
            retry_base_delay: Duration::ZERO,
        },
        Arc::new(ProviderRegistry::new("script", provider.clone())),
        Some(trace.clone()),
    )?;
    let request = ModelRequest {
        model: "script/model".into(),
        system_prompt: "base".into(),
        messages: vec![
            root_message("cold", "applied memory"),
            root_message("source", "selected evidence"),
        ],
        ..Default::default()
    };
    let original = serde_json::to_vec(&request.messages)?;
    models.record_fork_context("root", &request, &[0]);
    models
        .summarize_fork(
            summary("root"),
            EpisodeFork {
                request,
                first_message_id: "source".into(),
                last_message_id: "source".into(),
                provider: provider.clone(),
                max_request_bytes: 100_000,
                context_window: Some(100_000),
                output_reserve_tokens: 100,
                dispatched: true,
            },
        )
        .await?;
    trace.flush(Duration::from_secs(3))?;
    let status = trace.status();
    assert_eq!(status.dropped, 0);
    assert_eq!(status.write_errors, 0);
    let rows: Vec<Value> = std::fs::read_to_string(path)?
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).map(|envelope| envelope["record"].clone()))
        .collect::<std::result::Result<_, _>>()?;
    let built: Vec<_> = rows
        .iter()
        .filter(|row| row["kind"] == "context_built")
        .collect();
    assert_eq!(built.len(), 1);
    assert_eq!(built[0]["metrics"]["request_kind"], "summary_fork");
    assert_eq!(built[0]["metrics"]["cold"]["message_count"], 1);
    assert_eq!(built[0]["metrics"]["history"]["message_count"], 3);
    let requests = provider.0.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(serde_json::to_vec(&requests[0].messages[..2])?, original);
    assert!(requests[0].session_id.is_empty());
    Ok(())
}

#[tokio::test]
async fn direct_approval_deciders_retain_host_observer_across_live_generations() -> Result<()> {
    use bubblepaw_core::{DecisionRequest, Question, QuestionKind, ReasoningEffort};
    use bubblepaw_runner::{
        config::{EffectiveConfig, LoadOptions},
        provider::LiveProviderConfig,
    };
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("config.toml");
    let effective = EffectiveConfig::load_with_file(
        LoadOptions {
            workspace: temp.path().into(),
            home: None,
            custom_file: Some(path.clone()),
            env: vec![],
            cli: vec![],
        },
        &path,
        "[model]\nmain='fixture/model'\nprovider='fixture'\n[providers.fixture]\nkind='scripted'\n",
    )?;
    let live = Arc::new(LiveProviderConfig::new(effective.clone())?);
    let trace_path = temp.path().join("trace.jsonl");
    let trace = JsonlTrace::open(TraceConfig {
        path: trace_path.clone(),
        ..Default::default()
    })?;
    let _models = HttpEpisodeModels::from_live(
        EpisodeModelConfig {
            roles: ModelRoles::for_models("fixture/model".into(), String::new()),
            retry_base_delay: Duration::ZERO,
        },
        live.clone(),
        Some(trace.clone()),
    )?;
    let request = DecisionRequest {
        model: "llm/fixture/model".into(),
        input: "tool state".into(),
        reasoning_effort: ReasoningEffort::Low,
        session_id: Some("root".into()),
        questions: vec![Question {
            name: "approve".into(),
            instructions: "May this run?".into(),
            kind: QuestionKind::Predicate,
        }],
    };
    // The scripted demo is not a decision model; its output need not parse.
    // The request must still emit its context observation before the adapter.
    let _ = live
        .snapshot()
        .registry
        .decide_with_reasoning(request.clone(), CancellationToken::new())
        .await;
    live.snapshot().registry.close_session("root").await?;
    let next = live.prepare(effective)?;
    live.apply_prepared(next);
    let _ = live
        .snapshot()
        .registry
        .decide_with_reasoning(request, CancellationToken::new())
        .await;
    trace.flush(Duration::from_secs(3))?;
    let status = trace.status();
    assert_eq!(status.dropped, 0);
    assert_eq!(status.write_errors, 0);
    let rows: Vec<Value> = std::fs::read_to_string(trace_path)?
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).map(|envelope| envelope["record"].clone()))
        .collect::<std::result::Result<_, _>>()?;
    let built: Vec<_> = rows
        .iter()
        .filter(|row| row["kind"] == "context_built")
        .collect();
    assert_eq!(
        built.len(),
        2,
        "direct approval calls must inherit tracing without episode task-local scope"
    );
    assert!(
        built
            .iter()
            .all(|row| row["metrics"]["request_kind"] == "decider:approve")
    );
    assert_eq!(built[0]["metrics"]["classification"]["kind"], "first");
    assert_eq!(built[1]["metrics"]["classification"]["kind"], "append");
    Ok(())
}
