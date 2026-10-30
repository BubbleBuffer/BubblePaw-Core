#[path = "support/episode_fixture.rs"]
mod episode_fixture;
use anyhow::{Result, bail};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, EventKind, Harness, HarnessConfig, HarnessEvent, ModelRequest,
    ModelResponse, Provider, RunOptions, Store, TextCallback, ToolRegistry, Usage,
};
use bubblepaw_runner::{
    episode_background::EpisodeBackground,
    episode_models::{
        ChapterInput, ChapterResult, EpisodeModels, SplitInput, SplitResult, SummaryInput,
        SummaryResult,
    },
    summarization::{SummarizingContext, SummaryConfig},
    usage::{UsageSettings, session_usage},
};
use episode_fixture::new_episode_worker;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
#[path = "common/hook.rs"]
mod hook;

struct MainModel;
#[async_trait]
impl Provider for MainModel {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: " abc".repeat(35_000),
            ..Default::default()
        })
    }
}
struct Models {
    summaries: AtomicUsize,
    fail: bool,
    calls: Mutex<Vec<String>>,
    summarized: tokio::sync::Notify,
}
#[async_trait]
impl EpisodeModels for Models {
    async fn decide(&self, input: SplitInput) -> Result<SplitResult> {
        self.calls.lock().unwrap().push(input.session_id.clone());
        let mut probabilities = vec![0.0; input.options.len()];
        probabilities.push(1.0);
        Ok(SplitResult {
            choice: None,
            probabilities,
            model: "fake-decider".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
    async fn summarize(&self, input: SummaryInput) -> Result<SummaryResult> {
        self.calls.lock().unwrap().push(input.session_id);
        self.summaries.fetch_add(1, Ordering::SeqCst);
        self.summarized.notify_one();
        if self.fail {
            bail!("scripted exhausted model failure");
        }
        Ok(SummaryResult {
            title: "Episode".into(),
            summary: "# Episode\nRetained facts.".into(),
            model: "fake-summary".into(),
            usage: Usage::default(),
            reasoning: Some("fake reasoning".into()),
        })
    }
    async fn close_chapter(&self, input: ChapterInput) -> Result<ChapterResult> {
        self.calls.lock().unwrap().push(input.session_id);
        bail!("No split was scripted")
    }
}

async fn exercise(fail: bool) -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("event hook")?;
    let provider = Arc::new(MainModel);
    let summary = Arc::new(SummarizingContext::new(
        store.clone(),
        provider.clone(),
        SummaryConfig {
            threshold_cap: 100_000,
            ..Default::default()
        },
        None,
    )?);
    let models = Arc::new(Models {
        summaries: AtomicUsize::new(0),
        fail,
        calls: Mutex::new(Vec::new()),
        summarized: tokio::sync::Notify::new(),
    });
    let worker = new_episode_worker(store.clone(), models.clone(), summary.clone());
    let harness = Harness::new(
        store.clone(),
        provider,
        ToolRegistry::new(),
        HarnessConfig {
            model: "fake-main".into(),
            context: summary.clone(),
            ..Default::default()
        },
    );
    let template = ModelRequest {
        model: "fake-main".into(),
        system_prompt: String::new(),
        messages: vec![],
        tools: vec![],
        max_output_tokens: None,
        ..Default::default()
    };
    let background = EpisodeBackground::start(
        harness.subscribe(),
        store.clone(),
        summary.clone(),
        worker.clone(),
        hook::fixed_template(template.clone()),
    );
    harness
        .prompt(&session.id, "first", RunOptions::default())
        .await?;
    // A graceful stop drains events: eligible source gathers below serving pressure.
    background.shutdown(Duration::from_secs(10)).await;
    assert!(models.summaries.load(Ordering::SeqCst) >= 1);
    assert_eq!(
        store.context_episodes(&session.id)?.len(),
        usize::from(!fail)
    );

    let worker = new_episode_worker(store.clone(), models.clone(), summary.clone());
    let background = EpisodeBackground::start(
        harness.subscribe(),
        store.clone(),
        summary.clone(),
        worker,
        hook::fixed_template(template.clone()),
    );
    harness
        .prompt(&session.id, "second", RunOptions::default())
        .await?;
    let history = store.messages(&session.id)?;
    let mut request = template;
    request.messages = history.clone();
    let before = hook::prepare_fresh(&*summary, Arc::new(MainModel), &session.id, request.clone())
        .await?
        .request;
    background.shutdown(Duration::from_secs(10)).await;
    assert!(models.summaries.load(Ordering::SeqCst) >= 1);
    assert_eq!(
        store.context_episodes(&session.id)?.len(),
        2 * usize::from(!fail)
    );
    assert!(store.context_chapters(&session.id)?.is_empty());
    assert_eq!(
        serde_json::to_value(store.messages(&session.id)?)?,
        serde_json::to_value(history)?
    );
    let after = hook::prepare_fresh(&*summary, Arc::new(MainModel), &session.id, request)
        .await?
        .request;
    assert_eq!(serde_json::to_vec(&before)?, serde_json::to_vec(&after)?);
    Ok(())
}

#[tokio::test]
async fn committed_events_wake_worker_without_changing_provider_view() -> Result<()> {
    exercise(false).await
}
#[tokio::test]
async fn background_model_failure_does_not_fail_main_run() -> Result<()> {
    exercise(true).await
}

/// Summarizes slowly with known usage and records shutdown diagnostics.
struct SlowModels {
    delay: Duration,
    aborted: Mutex<Option<(usize, usize)>>,
}
#[async_trait]
impl EpisodeModels for SlowModels {
    async fn decide(&self, input: SplitInput) -> Result<SplitResult> {
        let mut probabilities = vec![0.0; input.options.len()];
        probabilities.push(1.0);
        Ok(SplitResult {
            choice: None,
            probabilities,
            model: "fake-decider".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
    async fn summarize(&self, _: SummaryInput) -> Result<SummaryResult> {
        tokio::time::sleep(self.delay).await;
        Ok(SummaryResult {
            title: "Episode".into(),
            summary: "# Episode
Retained facts."
                .into(),
            model: "fake-summary".into(),
            usage: Usage {
                input_tokens: 1234,
                output_tokens: 56,
                ..Usage::default()
            },
            reasoning: None,
        })
    }
    async fn close_chapter(&self, _: ChapterInput) -> Result<ChapterResult> {
        bail!("No split was scripted")
    }
    fn record_shutdown_abort(&self, aborted: usize, lost: usize, _: Duration) {
        *self.aborted.lock().unwrap() = Some((aborted, lost));
    }
}

async fn finish_one_shot(
    delay: Duration,
    drain: Duration,
) -> Result<(Store, String, Arc<SlowModels>)> {
    let store = Store::in_memory()?;
    let session = store.create_session("one-shot exit")?;
    let provider = Arc::new(MainModel);
    let summary = Arc::new(SummarizingContext::new(
        store.clone(),
        provider.clone(),
        SummaryConfig {
            threshold_cap: 100_000,
            ..Default::default()
        },
        None,
    )?);
    let models = Arc::new(SlowModels {
        delay,
        aborted: Mutex::new(None),
    });
    let worker = new_episode_worker(store.clone(), models.clone(), summary.clone());
    let harness = Harness::new(
        store.clone(),
        provider,
        ToolRegistry::new(),
        HarnessConfig {
            model: "fake-main".into(),
            context: summary.clone(),
            ..Default::default()
        },
    );
    let background = EpisodeBackground::start(
        harness.subscribe(),
        store.clone(),
        summary,
        worker,
        hook::fixed_template(ModelRequest {
            model: "fake-main".into(),
            ..Default::default()
        }),
    );
    harness
        .prompt(&session.id, "first", RunOptions::default())
        .await?;
    // The run has finished; the gather it woke is still waiting on its model.
    background.shutdown(drain).await;
    Ok((store, session.id, models))
}

#[tokio::test]
async fn one_shot_drain_commits_a_slow_in_flight_gather_into_session_usage() -> Result<()> {
    let (store, session, models) = finish_one_shot(
        Duration::from_millis(500),
        EpisodeBackground::ONE_SHOT_DRAIN,
    )
    .await?;
    assert_eq!(store.context_episodes(&session)?.len(), 1);
    let usage = session_usage(&store, &session, &UsageSettings::default())?;
    assert_eq!(usage.own.tokens.input_tokens, 1234);
    assert_eq!(usage.own.tokens.output_tokens, 56);
    assert_eq!(*models.aborted.lock().unwrap(), None);
    Ok(())
}

#[tokio::test]
async fn timed_out_drain_traces_the_aborted_call_before_aborting() -> Result<()> {
    let (store, session, models) =
        finish_one_shot(Duration::from_secs(30), Duration::from_millis(200)).await?;
    assert!(store.context_episodes(&session)?.is_empty());
    assert_eq!(*models.aborted.lock().unwrap(), Some((1, 0)));
    let usage = session_usage(&store, &session, &UsageSettings::default())?;
    assert_eq!(usage.own.tokens.input_tokens, 0);
    Ok(())
}

async fn all_sessions_are_scheduled(lagged: bool, immediate_shutdown: bool) -> Result<()> {
    let store = Store::in_memory()?;
    let visible = store.create_session("ordinary chat")?;
    let hidden = store.create_session_with_visibility(
        "internal worker",
        Default::default(),
        Some(&visible.id),
        true,
    )?;
    let native = store.create_session_with_visibility(
        "native child",
        Default::default(),
        Some(&visible.id),
        false,
    )?;
    let provider = Arc::new(MainModel);
    // Populate all session classes with enough source for an episode.
    // The listener starts afterward, so the queued wake-ups are deterministic.
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            model: "fake-main".into(),
            ..Default::default()
        },
    );
    for session in [&visible, &native, &hidden] {
        for task in ["first", "second"] {
            harness
                .prompt(&session.id, task, RunOptions::default())
                .await?;
        }
    }
    let summary = Arc::new(SummarizingContext::new(
        store.clone(),
        provider,
        SummaryConfig {
            threshold_cap: 100_000,
            ..Default::default()
        },
        None,
    )?);
    let models = Arc::new(Models {
        summaries: AtomicUsize::new(0),
        fail: false,
        calls: Mutex::new(Vec::new()),
        summarized: tokio::sync::Notify::new(),
    });
    let worker = new_episode_worker(store.clone(), models.clone(), summary.clone());
    let (events, receiver) = tokio::sync::broadcast::channel(if lagged { 1 } else { 16 });
    let wake = |session: &str, sequence| HarnessEvent {
        sequence,
        durable: true,
        origin: None,
        session_id: session.into(),
        run_id: String::new(),
        created_at: String::new(),
        kind: EventKind::SessionSettled {},
    };
    events.send(wake(&hidden.id, 1))?;
    events.send(wake(&native.id, 2))?;
    events.send(wake(&visible.id, 3))?;
    // Capacity one guarantees the lag branch before the surviving visible wake-up.
    let background = EpisodeBackground::start(
        receiver,
        store.clone(),
        summary,
        worker,
        hook::fixed_template(ModelRequest {
            model: "fake-main".into(),
            ..Default::default()
        }),
    );
    if !immediate_shutdown {
        tokio::time::timeout(Duration::from_secs(10), models.summarized.notified()).await?;
    }
    background.shutdown(Duration::from_secs(10)).await;
    assert_eq!(store.context_episodes(&visible.id)?.len(), 2);
    assert_eq!(store.context_episodes(&native.id)?.len(), 2);
    assert_eq!(store.context_episodes(&hidden.id)?.len(), 2);
    assert_eq!(store.context_subject_chapters(&hidden.id)?.len(), 1);
    assert!(!store.context_decisions(&hidden.id)?.is_empty());
    assert_eq!(models.summaries.load(Ordering::SeqCst), 6);
    assert!(
        models
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|session| session == &hidden.id)
    );
    Ok(())
}

#[tokio::test]
async fn shared_host_schedules_hidden_worker_wake_events() -> Result<()> {
    all_sessions_are_scheduled(false, false).await
}

#[tokio::test]
async fn lagged_events_rescan_visible_and_hidden_sessions() -> Result<()> {
    all_sessions_are_scheduled(true, false).await
}

#[tokio::test]
async fn shutdown_drain_recovers_lagged_hidden_worker_events() -> Result<()> {
    all_sessions_are_scheduled(true, true).await
}

#[test]
fn removed_episode_activation_flag_is_rejected() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_bubblepaw"))
        .args(["--episodes", "sessions"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument '--episodes'"));
}

#[tokio::test]
async fn runner_explicit_episode_policy_gathers_responses_source_and_applies_at_pressure()
-> Result<()> {
    use axum::{Json, Router, routing::post};
    let episode_prompt = include_str!("../src/summary_prompt_v2.txt")
        .split("{target}")
        .next()
        .unwrap();
    use serde_json::{Value, json};

    let compactions = Arc::new(AtomicUsize::new(0));
    let ordinary = Arc::new(AtomicUsize::new(0));
    let summary_calls = compactions.clone();
    let ordinary_calls = ordinary.clone();
    let app = Router::new().route("/responses", post(move |Json(body): Json<Value>| {
        let summary_calls = summary_calls.clone();
        let ordinary_calls = ordinary_calls.clone();
        async move {
            let text = if body["instructions"].as_str().is_some_and(|s| s.starts_with(episode_prompt)) {
                summary_calls.fetch_add(1, Ordering::SeqCst);
                "# Existing compaction\nThe user's prior requests and work are retained.".to_owned()
            } else if body["instructions"].as_str().is_some_and(|s| s.contains("Which numbered boundary")) {
                r#"{"choice":"none"}"#.to_owned()
            } else {
                ordinary_calls.fetch_add(1, Ordering::SeqCst);
                " abc".repeat(35_000)
            };
            let event = json!({"type":"response.completed","response":{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}]}});
            ([("content-type", "text/event-stream")], format!("data: {event}\n\n"))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    struct AbortServer(tokio::task::JoinHandle<std::io::Result<()>>);
    impl Drop for AbortServer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _server = AbortServer(server);
    let root = tempfile::tempdir()?;
    // Responses does not support a full-prefix fork. Select the documented
    // episode-only policy explicitly; unsupported Fork must never substitute it.
    std::fs::create_dir_all(root.path().join(".bubblepaw"))?;
    std::fs::write(
        root.path().join(".bubblepaw/config.toml"),
        "[context.lcm]\nsummary_input = \"episode\"\n",
    )?;
    let db = root.path().join("archive.sqlite");
    let session = {
        let store = Store::open(&db)?;
        store.create_session("episodes always on")?.id
    };
    for index in 0..4 {
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_bubblepaw"))
            .args(["--provider", "api", "--read-only", "--workspace"])
            .arg(root.path())
            .arg("--db")
            .arg(&db)
            .args([
                "--base-url",
                &base_url,
                "--model",
                "fake-main",
                // The synthetic OpenAI key would otherwise select the Luna default.
                "--episode-summary-base-url=",
                "--episode-summary-reasoning-fields={}",
                "--decider",
                "llm/api/fake-main",
                "--compact-threshold-tokens",
                "100000",
                "run",
                &format!("request {index}"),
                "--session",
                &session,
            ])
            .env("OPENAI_API_KEY", "synthetic-key")
            .env_remove("OPENCODE_API_KEY")
            .env_remove("OPENROUTER_API_KEY")
            .output()
            .await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let store = Store::open(&db)?;
        assert!(
            store.context_chapters(&session)?.is_empty(),
            "New v4 writes are gone"
        );
        assert!(!store.context_episodes(&session)?.is_empty());
        assert!(!store.context_subject_chapters(&session)?.is_empty());
        if index == 0 {
            assert!(
                store.context_decisions(&session)?.is_empty(),
                "The first episode seeds the chapter; later episodes assess continuation"
            );
        } else {
            assert!(!store.context_decisions(&session)?.is_empty());
        }
        if index == 1 {
            let estimator = SummarizingContext::new(
                store.clone(),
                Arc::new(MainModel),
                SummaryConfig {
                    threshold_cap: 100_000,
                    ..Default::default()
                },
                None,
            )?;
            let request = ModelRequest {
                model: "fake-main".into(),
                system_prompt: String::new(),
                messages: store.messages(&session)?,
                tools: vec![],
                max_output_tokens: None,
                ..Default::default()
            };
            assert!(estimator.estimated_tokens(&request) >= 50_000);
            assert!(store.context_chapters(&session)?.is_empty());
        }
        if index == 3 {
            assert!(store.context_view(&session)?.is_some());
        }
    }
    assert_eq!(ordinary.load(Ordering::SeqCst), 4);
    assert!(compactions.load(Ordering::SeqCst) >= 3);
    Ok(())
}

// These tests exercise gathering/apply with deterministic episode-only models.
// Fork transport is covered separately by captured-request adapter tests.
