use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, FullHistory, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider,
    RunOptions, Store, TextCallback, ToolRegistry, Usage,
};
use bubblepaw_runner::usage::{UsageContext, UsageSettings, session_usage};
use std::sync::{Arc, Mutex};

struct Responses(Mutex<std::collections::VecDeque<ModelResponse>>);
#[async_trait]
impl Provider for Responses {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected provider request"))
    }
}

struct Metered(Mutex<Option<Usage>>);

struct Outcomes(Mutex<std::collections::VecDeque<Result<ModelResponse>>>);
#[async_trait]
impl Provider for Outcomes {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected request")
    }
}

struct AwaitCancellation {
    first: Mutex<Option<ModelResponse>>,
    waiting: tokio::sync::Notify,
}
#[async_trait]
impl Provider for AwaitCancellation {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        if let Some(response) = self.first.lock().unwrap().take() {
            return Ok(response);
        }
        self.waiting.notify_one();
        cancellation.cancelled().await;
        anyhow::bail!("cancelled")
    }
}

fn schema_options() -> Result<RunOptions> {
    Ok(RunOptions {
        output_schema: Some(bubblepaw_core::JsonSchema::new(
            "answer",
            serde_json::json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}),
        )?),
        ..Default::default()
    })
}

#[tokio::test]
async fn failed_rounds_retain_known_usage_once_in_recursive_totals() -> Result<()> {
    let store = Store::in_memory()?;
    let parent = store.create_session("parent")?;
    metered_harness(&store, 10, "local/model", true)
        .prompt(&parent.id, "parent", RunOptions::default())
        .await?;
    let child = store.fork(&parent.id, "failed child")?;
    let provider = Arc::new(Outcomes(Mutex::new(std::collections::VecDeque::from([
        Ok(ModelResponse {
            text: "invalid JSON".into(),
            usage: Some(tokens(100)),
            ..Default::default()
        }),
        Err(bubblepaw_core::ProviderError::new(
            bubblepaw_core::ProviderErrorKind::Fatal,
            "unavailable tool",
        )
        .with_usage(tokens(200))
        .into()),
    ]))));
    let harness = Harness::new(
        store.clone(),
        provider,
        ToolRegistry::new(),
        HarnessConfig {
            model: "local/model".into(),
            context: Arc::new(UsageContext::new(store.clone(), Arc::new(FullHistory))),
            ..Default::default()
        },
    );
    let run = harness
        .prompt(&child.id, "answer", schema_options()?)
        .await?;
    assert_eq!(run.status, "failed");
    let mut expected = tokens(100);
    expected.add(&tokens(200));
    assert_eq!(run.usage, Some(expected.clone()));
    assert_eq!(store.run(&run.id)?.usage, Some(expected.clone()));
    let grandchild = store.fork(&child.id, "grandchild")?;
    metered_harness(&store, 300, "local/model", true)
        .prompt(&grandchild.id, "grandchild", RunOptions::default())
        .await?;
    let report = session_usage(&store, &parent.id, &UsageSettings::default())?;
    assert_eq!(report.total.tokens.input_tokens, 610);
    assert_eq!(report.total.recorded_entries, 3);
    assert_eq!(report.total.entries_without_usage, 0);
    assert!(report.total.partial);
    let child_report = session_usage(&store, &child.id, &UsageSettings::default())?;
    assert_eq!(child_report.own.tokens, expected);
    assert_eq!(child_report.total.tokens.input_tokens, 600);
    assert_eq!(child_report.context.last_request_input_tokens, Some(100));
    Ok(())
}

#[tokio::test]
async fn cancellation_retains_accepted_usage_without_inventing_inflight_usage() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("cancelled usage")?;
    let provider = Arc::new(AwaitCancellation {
        first: Mutex::new(Some(ModelResponse {
            text: "invalid JSON".into(),
            usage: Some(tokens(100)),
            ..Default::default()
        })),
        waiting: tokio::sync::Notify::new(),
    });
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            context: Arc::new(UsageContext::new(store.clone(), Arc::new(FullHistory))),
            ..Default::default()
        },
    );
    harness.start(&session.id, "answer", schema_options()?)?;
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.waiting.notified(),
    )
    .await?;
    assert!(harness.cancel(&session.id));
    let run = harness.wait_for_idle(&session.id).await?;
    assert_eq!(run.status, "cancelled");
    assert_eq!(run.usage, Some(tokens(100)));
    let report = session_usage(&store, &session.id, &UsageSettings::default())?;
    assert_eq!(report.own.tokens, tokens(100));
    assert_eq!(report.context.last_request_input_tokens, Some(100));

    let unknown = store.create_session("unknown failure")?;
    let harness = Harness::new(
        store.clone(),
        Arc::new(Outcomes(Mutex::new(std::collections::VecDeque::from([
            Err(anyhow::anyhow!("unmetered failure")),
        ])))),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    let run = harness
        .prompt(&unknown.id, "fail", RunOptions::default())
        .await?;
    assert!(run.usage.is_none());
    Ok(())
}
#[async_trait]
impl Provider for Metered {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: "complete".into(),
            usage: self.0.lock().unwrap().clone(),
            ..Default::default()
        })
    }
}
/// Exact recognized model ID (provider route prefix allowed), from the shared dated table.
const PRICED: &str = "opencode-messages/claude-haiku-5-5";

fn tokens(input: u64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: 10,
        cache_read_tokens: input / 4,
        cache_creation_tokens: input / 4,
        reasoning_tokens: Some(3),
    }
}
fn metered_harness(store: &Store, input: u64, model: &str, observe: bool) -> Harness {
    let mut config = HarnessConfig {
        model: model.into(),
        ..Default::default()
    };
    if observe {
        config.context = Arc::new(UsageContext::new(store.clone(), Arc::new(FullHistory)));
    }
    Harness::new(
        store.clone(),
        Arc::new(Metered(Mutex::new(Some(tokens(input))))),
        ToolRegistry::new(),
        config,
    )
}

#[tokio::test]
async fn descendants_are_counted_once_and_inherited_history_is_not_usage() -> Result<()> {
    let store = Store::in_memory()?;
    let parent = store.create_session("parent")?;
    metered_harness(&store, 100, PRICED, true)
        .prompt(&parent.id, "parent", RunOptions::default())
        .await?;
    let child = store.fork(&parent.id, "child")?;
    metered_harness(&store, 200, PRICED, true)
        .prompt(&child.id, "child", RunOptions::default())
        .await?;
    let grandchild = store.fork(&child.id, "grandchild")?;
    metered_harness(&store, 300, "unknown/model", true)
        .prompt(&grandchild.id, "grandchild", RunOptions::default())
        .await?;
    let unrelated = store.create_session("unrelated")?;
    metered_harness(&store, 9000, PRICED, true)
        .prompt(&unrelated.id, "unrelated", RunOptions::default())
        .await?;
    let settings = UsageSettings {
        context_window_tokens: Some(100_000),
        compact_threshold_tokens: 90_000,
        ..Default::default()
    };
    let before = store.storage_stats()?.transaction_count;
    let report = session_usage(&store, &parent.id, &settings)?;
    assert_eq!(
        store.storage_stats()?.transaction_count,
        before,
        "reporting is read-only"
    );
    assert_eq!(report.own.tokens.input_tokens, 100);
    assert_eq!(report.total.tokens.input_tokens, 600);
    assert_eq!(report.total.tokens.output_tokens, 30);
    assert_eq!(report.total.tokens.cache_read_tokens, 150);
    assert_eq!(report.total.tokens.cache_creation_tokens, 150);
    assert_eq!(report.total.tokens.reasoning_tokens, Some(9));
    assert_eq!(report.descendants.len(), 2);
    assert_eq!(report.context.last_request_input_tokens, Some(100));
    // docs/lcm-contract.md "Apply": min(100k window, 90k physical cap) minus the
    // 16k reserve, deducted exactly once.
    assert_eq!(report.context.compaction_trigger_tokens, 74_000);
    assert_eq!(report.context.fill_ratio, Some(0.001));
    // Haiku off-peak base rates per request (input 100 = 50 uncached, 25 read, 25 write, 10 output):
    // 5 + 0.25 + 3.125 + 5 = 13.375 micro-USD; the child (input 200) adds 21.75.
    assert!((report.own.estimated_cost_usd.unwrap() - 13.375e-6).abs() < 1e-12);
    assert!((report.total.estimated_cost_usd.unwrap() - 35.125e-6).abs() < 1e-12);
    assert_eq!(report.total.entries_without_pricing, 1);
    assert!(report.total.partial);
    let child_report = session_usage(&store, &child.id, &settings)?;
    assert_eq!(child_report.own.tokens.input_tokens, 200);
    assert_eq!(child_report.total.tokens.input_tokens, 500);
    let grandchild_report = session_usage(&store, &grandchild.id, &settings)?;
    assert!(grandchild_report.own.estimated_cost_usd.is_none());
    Ok(())
}

#[tokio::test]
async fn latest_request_is_separate_from_totals_and_unknown_is_not_zero() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("usage")?;
    let provider = Arc::new(Metered(Mutex::new(Some(tokens(100)))));
    let config = HarnessConfig {
        model: "local/model".into(),
        context: Arc::new(UsageContext::new(store.clone(), Arc::new(FullHistory))),
        ..Default::default()
    };
    let harness = Harness::new(store.clone(), provider.clone(), ToolRegistry::new(), config);
    harness
        .prompt(&session.id, "one", RunOptions::default())
        .await?;
    *provider.0.lock().unwrap() = Some(tokens(200));
    harness
        .prompt(&session.id, "two", RunOptions::default())
        .await?;
    let settings = UsageSettings {
        auto_summarize: false,
        ..Default::default()
    };
    let report = session_usage(&store, &session.id, &settings)?;
    assert_eq!(report.context.last_request_input_tokens, Some(200));
    assert_eq!(report.own.tokens.input_tokens, 300);
    assert!(report.context.window_tokens.is_none());
    assert!(report.context.fill_ratio.is_none());
    assert!(!report.context.auto_compaction_enabled);
    *provider.0.lock().unwrap() = None;
    harness
        .prompt(&session.id, "unknown", RunOptions::default())
        .await?;
    let report = session_usage(&store, &session.id, &settings)?;
    assert!(report.context.last_request_input_tokens.is_none());
    assert!(report.context.last_request_observed_at.is_some());
    assert!(report.own.estimated_cost_usd.is_none());
    let old = store.create_session("historical")?;
    metered_harness(&store, 400, "local/model", false)
        .prompt(&old.id, "old", RunOptions::default())
        .await?;
    let historical = session_usage(&store, &old.id, &settings)?;
    assert_eq!(historical.own.tokens.input_tokens, 400);
    assert!(historical.context.last_request_input_tokens.is_none());
    Ok(())
}

#[path = "usage/submission.rs"]
mod submission;

#[tokio::test]
async fn physical_request_metadata_overrides_aggregate_and_explicit_unknown_does_not_fallback()
-> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("physical request")?;
    let provider = Arc::new(Responses(Mutex::new(std::collections::VecDeque::from([
        ModelResponse {
            text: "joined reply".into(),
            usage: Some(tokens(300)),
            last_request_usage: Some(bubblepaw_core::LastRequestUsage {
                input_tokens: Some(200),
                output_tokens: Some(10),
            }),
            ..Default::default()
        },
        ModelResponse {
            text: "joined unknown final request".into(),
            usage: Some(tokens(100)),
            last_request_usage: Some(bubblepaw_core::LastRequestUsage::default()),
            ..Default::default()
        },
    ]))));
    let config = HarnessConfig {
        model: "local/model".into(),
        context: Arc::new(UsageContext::new(store.clone(), Arc::new(FullHistory))),
        ..Default::default()
    };
    let harness = Harness::new(store.clone(), provider, ToolRegistry::new(), config);
    harness
        .prompt(&session.id, "known", RunOptions::default())
        .await?;
    assert_eq!(
        session_usage(&store, &session.id, &UsageSettings::default())?
            .context
            .last_request_input_tokens,
        Some(200)
    );
    harness
        .prompt(&session.id, "unknown", RunOptions::default())
        .await?;
    let report = session_usage(&store, &session.id, &UsageSettings::default())?;
    assert!(report.context.last_request_input_tokens.is_none());
    assert_eq!(report.own.tokens.input_tokens, 400);
    Ok(())
}

#[tokio::test]
async fn cached_window_is_exact_local_codex_metadata_and_price_gaps_stay_unknown() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let cache = dir.path().join("models_cache.json");
    std::fs::write(
        &cache,
        r#"{"models":[{"slug":"model","context_window":120000}]}"#,
    )?;
    let store = Store::in_memory()?;
    let session = store.create_session("cached")?;
    metered_harness(&store, 100, "codex/model", true)
        .prompt(&session.id, "hello", RunOptions::default())
        .await?;
    let settings = UsageSettings {
        codex_models_cache: Some(cache),
        ..Default::default()
    };
    let report = session_usage(&store, &session.id, &settings)?;
    assert_eq!(report.context.window_tokens, Some(120_000));
    assert_eq!(report.context.compaction_trigger_tokens, 104_000);
    assert!(
        report.own.estimated_cost_usd.is_none(),
        "an unknown model is never priced"
    );
    // A recognized model with no published rate stays unknown too, never zero or guessed.
    let unpriced = store.create_session("unpriced")?;
    metered_harness(&store, 100, "typesafe/jev-1.13", true)
        .prompt(&unpriced.id, "hello", RunOptions::default())
        .await?;
    let unpriced_report = session_usage(&store, &unpriced.id, &settings)?;
    assert!(unpriced_report.own.estimated_cost_usd.is_none());
    assert_eq!(unpriced_report.own.entries_without_pricing, 1);
    let other = store.create_session("other route")?;
    metered_harness(&store, 100, "other/model", true)
        .prompt(&other.id, "hello", RunOptions::default())
        .await?;
    assert!(
        session_usage(&store, &other.id, &settings)?
            .context
            .window_tokens
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn retained_summary_usage_is_added_once_without_mutating_original_history() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("summary usage")?;
    metered_harness(&store, 100, "main/model", true)
        .prompt(&session.id, "original", RunOptions::default())
        .await?;
    let original = store.messages(&session.id)?;
    store.save_context_checkpoint(&bubblepaw_core::ContextCheckpoint {
        id: "fixture-summary".into(),
        session_id: session.id.clone(),
        through_message_id: original.last().unwrap().id.clone(),
        model: "helper/model".into(),
        summary: "retained fixture summary".into(),
        files_touched: Vec::new(),
        created_at: chrono::Utc::now().to_rfc3339(),
        usage: Some(tokens(60)),
    })?;
    let before = store.storage_stats()?.transaction_count;
    let report = session_usage(&store, &session.id, &UsageSettings::default())?;
    assert_eq!(report.own.tokens.input_tokens, 160);
    assert_eq!(report.own.recorded_entries, 2);
    assert_eq!(report.total.tokens.output_tokens, 20);
    assert_eq!(store.storage_stats()?.transaction_count, before);
    assert_eq!(store.messages(&session.id)?, original);
    Ok(())
}

#[tokio::test]
async fn reporting_an_archived_root_does_not_restore_storage() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("archive")?;
    metered_harness(&store, 100, "main/model", true)
        .prompt(&session.id, "original", RunOptions::default())
        .await?;
    assert!(store.archive_chat(&session.id)?);
    let before = store.storage_stats()?.transaction_count;
    assert!(session_usage(&store, &session.id, &UsageSettings::default()).is_err());
    assert!(store.active_session_metadata(&session.id)?.is_none());
    assert_eq!(store.storage_stats()?.transaction_count, before);
    Ok(())
}

#[tokio::test]
async fn latest_row_is_bounded_and_observation_overhead_is_measured() -> Result<()> {
    async fn measure(observe: bool) -> Result<(u64, u64, u64)> {
        let dir = tempfile::tempdir()?;
        let store = Store::open_data_dir(dir.path())?;
        let session = store.create_session("measurement")?;
        let harness = metered_harness(&store, 100, "local/model", observe);
        let before = store.storage_stats()?;
        for _ in 0..32 {
            harness
                .prompt(&session.id, "bounded fixture", RunOptions::default())
                .await?;
        }
        let after = store.storage_stats()?;
        let state = store.host_state(&session.id, "usage.last_request")?;
        if observe {
            assert!(serde_json::to_vec(&state)?.len() < 256);
        } else {
            assert!(state.is_none());
        }
        assert_eq!(
            store.messages(&session.id)?.len(),
            64,
            "telemetry adds no history messages"
        );
        Ok((
            after.transaction_count - before.transaction_count,
            after.database_bytes.saturating_sub(before.database_bytes),
            after.wal_bytes.saturating_sub(before.wal_bytes),
        ))
    }
    let baseline = measure(false).await?;
    let observed = measure(true).await?;
    assert_eq!(
        observed.0 - baseline.0,
        32,
        "one extra transaction per completed response"
    );
    println!(
        "usage observation 32 responses: baseline tx/db-growth/wal-growth={baseline:?}; observed={observed:?}"
    );
    Ok(())
}
