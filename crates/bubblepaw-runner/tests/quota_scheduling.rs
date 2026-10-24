use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider,
    ProviderError, ProviderErrorKind, QuotaReading, QuotaSource, RunOptions, Store, TextCallback,
    ToolRegistry,
};
use bubblepaw_runner::{
    provider::ProviderRegistry,
    quota::{LimitAction, QuotaHost, classify_limit},
    quota_recovery::{QuotaRecovery, root_resume},
};
use chrono::{DateTime, Utc};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn exhausted(reset: DateTime<Utc>) -> QuotaReading {
    QuotaReading {
        provider: "p".into(),
        account: "shared".into(),
        window: "five_hour".into(),
        used_percent: 100.0,
        resets_at: Some(reset.to_rfc3339()),
        observed_at: Utc::now().to_rfc3339(),
        source: QuotaSource::Rejection,
        balance: None,
    }
}

fn rejected(reset: Option<DateTime<Utc>>) -> anyhow::Error {
    ProviderError::new(
        ProviderErrorKind::RateLimit {
            resets_at: reset.map(|at| at.to_rfc3339()),
        },
        "request rejected",
    )
    .into()
}

fn rejected_with(readings: Vec<QuotaReading>) -> anyhow::Error {
    ProviderError::new(
        ProviderErrorKind::RateLimit { resets_at: None },
        "request rejected",
    )
    .with_quota(readings)
    .into()
}

fn observed(window: &str, reset: DateTime<Utc>, source: QuotaSource) -> QuotaReading {
    let mut reading = exhausted(reset);
    reading.window = window.into();
    reading.source = source;
    reading
}

#[test]
fn classifies_short_exhausted_long_terminal_unknown_and_max_wait() {
    let now = Utc::now();
    let max = Duration::from_secs(21600);
    assert_eq!(
        classify_limit(
            "shared",
            "claude-sonnet-5-5",
            &rejected(None),
            &[],
            now,
            max
        ),
        LimitAction::Short(Duration::ZERO)
    );
    let soon = now + chrono::Duration::seconds(2);
    assert_eq!(
        classify_limit(
            "shared",
            "claude-sonnet-5-5",
            &rejected(Some(soon)),
            &[],
            now,
            max
        ),
        LimitAction::Short(Duration::from_secs(2))
    );
    assert!(
        matches!(classify_limit("shared", "claude-sonnet-5-5", &rejected_with(vec![exhausted(soon)]), &[], now, max), LimitAction::Park { window, .. } if window == "five_hour")
    );
    let mut sdk_rejected = exhausted(soon);
    sdk_rejected.used_percent = 95.0;
    assert!(matches!(
        classify_limit(
            "shared",
            "claude-sonnet-5-5",
            &rejected_with(vec![sdk_rejected]),
            &[],
            now,
            max
        ),
        LimitAction::Park { .. }
    ));
    let later = now + chrono::Duration::minutes(30);
    assert!(matches!(
        classify_limit(
            "shared",
            "claude-sonnet-5-5",
            &rejected(Some(later)),
            &[],
            now,
            max
        ),
        LimitAction::Park { .. }
    ));
    let far = now + chrono::Duration::hours(7);
    let LimitAction::Fail(message) = classify_limit(
        "shared",
        "claude-sonnet-5-5",
        &rejected_with(vec![exhausted(far)]),
        &[],
        now,
        max,
    ) else {
        panic!("expected max wait failure");
    };
    assert!(message.contains(&format!("quota exhausted until {}", far.to_rfc3339())));
    assert!(message.contains("shared") && message.contains("five_hour"));
    let terminal =
        ProviderError::new(ProviderErrorKind::Fatal, "enforced_spend_limit_reached").into();
    assert!(matches!(
        classify_limit("shared", "claude-sonnet-5-5", &terminal, &[], now, max),
        LimitAction::Fail(_)
    ));
    let mut unknown = exhausted(soon);
    unknown.resets_at = None;
    assert!(
        matches!(classify_limit("shared", "claude-sonnet-5-5", &rejected(None), &[unknown], now, max), LimitAction::Fail(message) if message.contains("reset time unknown"))
    );
    let past = exhausted(now - chrono::Duration::seconds(1));
    assert_eq!(
        classify_limit(
            "shared",
            "claude-sonnet-5-5",
            &rejected(None),
            &[past],
            now,
            max
        ),
        LimitAction::Short(Duration::ZERO)
    );
}

#[test]
fn current_named_rejection_takes_precedence_over_cached_longer_windows() {
    let now = Utc::now();
    let soon = now + chrono::Duration::minutes(10);
    let week = now + chrono::Duration::days(3);
    let cached = vec![observed("seven_day", week, QuotaSource::UsageApi)];
    // SDK rejected events name a window even if its utilization is below 100%.
    let mut named = exhausted(soon);
    named.used_percent = 95.0;
    let error = rejected_with(vec![
        observed("seven_day", week, QuotaSource::SdkEvent),
        named,
    ]);
    assert_eq!(
        classify_limit(
            "shared",
            "sonnet",
            &error,
            &cached,
            now,
            Duration::from_secs(3600)
        ),
        LimitAction::Park {
            resume_at: soon,
            window: "five_hour".into()
        }
    );
    // The 429's own Codex/Anthropic exhausted headers outrank catalog readings.
    for window in ["primary", "tokens"] {
        let error = rejected_with(vec![observed(window, soon, QuotaSource::ResponseHeaders)]);
        assert_eq!(
            classify_limit(
                "shared",
                "sonnet",
                &error,
                &cached,
                now,
                Duration::from_secs(3600)
            ),
            LimitAction::Park {
                resume_at: soon,
                window: window.into()
            }
        );
    }
    // A provider may introduce a new named rejection before we know its kind.
    let error = rejected_with(vec![observed(
        "new_sdk_window",
        soon,
        QuotaSource::Rejection,
    )]);
    assert_eq!(
        classify_limit(
            "shared",
            "sonnet",
            &error,
            &cached,
            now,
            Duration::from_secs(3600)
        ),
        LimitAction::Park {
            resume_at: soon,
            window: "new_sdk_window".into()
        }
    );
}

#[test]
fn unnamed_rejection_uses_earliest_future_applicable_reset() {
    let now = Utc::now();
    let soon = now + chrono::Duration::minutes(10);
    for account_window in ["five_hour", "session", "primary", "requests"] {
        let mut unknown = observed("seven_day", soon, QuotaSource::UsageApi);
        unknown.resets_at = None;
        let readings = vec![
            observed(
                "weekly_all",
                now + chrono::Duration::days(3),
                QuotaSource::UsageApi,
            ),
            observed(
                "seven_day",
                now - chrono::Duration::seconds(1),
                QuotaSource::UsageApi,
            ),
            unknown,
            observed(account_window, soon, QuotaSource::UsageApi),
        ];
        assert_eq!(
            classify_limit(
                "shared",
                "sonnet",
                &rejected(None),
                &readings,
                now,
                Duration::from_secs(3600)
            ),
            LimitAction::Park {
                resume_at: soon,
                window: account_window.into()
            }
        );
    }
}

#[test]
fn fallback_scoped_windows_only_apply_to_the_called_model_family() {
    let now = Utc::now();
    let soon = now + chrono::Duration::minutes(10);
    let max = Duration::from_secs(3600);
    for (window, matching_model, other_model) in [
        (
            "weekly_scoped:sonnet",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
        ),
        (
            "weekly_scoped:Claude Opus",
            "provider/claude-opus-5-5",
            "sonnet",
        ),
        ("weekly_scoped:Fable", "claude-fable-5-1", "haiku"),
        ("weekly_scoped:claude-haiku-5-5", "haiku", "opus"),
        ("seven_day_sonnet", "sonnet", "haiku"),
        ("seven_day:opus", "opus", "sonnet"),
    ] {
        let readings = vec![observed(window, soon, QuotaSource::UsageApi)];
        assert_eq!(
            classify_limit(
                "shared",
                matching_model,
                &rejected(None),
                &readings,
                now,
                max
            ),
            LimitAction::Park {
                resume_at: soon,
                window: window.into()
            }
        );
        assert_eq!(
            classify_limit("shared", other_model, &rejected(None), &readings, now, max),
            LimitAction::Short(Duration::ZERO),
            "{window} must not restrict {other_model}"
        );
    }
}

#[test]
fn fallback_ignores_unknown_kinds_credits_other_accounts_and_unknown_scopes() {
    let now = Utc::now();
    let soon = now + chrono::Duration::minutes(10);
    let mut other = exhausted(soon);
    other.account = "other".into();
    let mut readings = vec![other];
    for window in [
        "unknown_future_limit",
        "unknown:sonnet",
        "weekly_scoped:unknown",
        "credits",
        "credits:sonnet",
    ] {
        let mut reading = observed(window, soon, QuotaSource::UsageApi);
        reading.resets_at = None;
        readings.push(reading);
    }
    assert_eq!(
        classify_limit(
            "shared",
            "sonnet",
            &rejected(None),
            &readings,
            now,
            Duration::from_secs(3600)
        ),
        LimitAction::Short(Duration::ZERO)
    );
    let error = rejected_with(vec![observed("credits", soon, QuotaSource::Rejection)]);
    assert_eq!(
        classify_limit(
            "shared",
            "sonnet",
            &error,
            &[],
            now,
            Duration::from_secs(3600)
        ),
        LimitAction::Short(Duration::ZERO)
    );
}

#[test]
fn no_evidence_stays_short_and_current_reset_overrides_cached_windows() {
    let now = Utc::now();
    let soon = now + chrono::Duration::seconds(2);
    let cached = vec![observed(
        "seven_day",
        now + chrono::Duration::days(3),
        QuotaSource::UsageApi,
    )];
    let max = Duration::from_secs(3600);
    assert_eq!(
        classify_limit("shared", "sonnet", &rejected(None), &[], now, max),
        LimitAction::Short(Duration::ZERO)
    );
    assert_eq!(
        classify_limit("shared", "sonnet", &rejected(Some(soon)), &cached, now, max),
        LimitAction::Short(Duration::from_secs(2))
    );
}

#[test]
fn unnamed_sdk_subscription_event_uses_fallback_or_stays_short() {
    let now = Utc::now();
    let soon = now + chrono::Duration::minutes(10);
    let mut unnamed = observed("subscription", soon, QuotaSource::Rejection);
    unnamed.resets_at = None;
    let error = rejected_with(vec![unnamed]);
    let max = Duration::from_secs(3600);
    assert_eq!(
        classify_limit("shared", "sonnet", &error, &[], now, max),
        LimitAction::Short(Duration::ZERO)
    );
    assert_eq!(
        classify_limit(
            "shared",
            "sonnet",
            &error,
            &[observed("five_hour", soon, QuotaSource::UsageApi)],
            now,
            max
        ),
        LimitAction::Park {
            resume_at: soon,
            window: "five_hour".into()
        }
    );
    let reset_only = rejected_with(vec![observed("subscription", soon, QuotaSource::Rejection)]);
    assert_eq!(
        classify_limit("shared", "sonnet", &reset_only, &[], now, max),
        LimitAction::Park {
            resume_at: soon,
            window: "subscription".into()
        }
    );
}

#[tokio::test]
async fn pre_request_exhausted_readings_never_block_a_successful_request() -> Result<()> {
    let host = QuotaHost::new(Store::in_memory()?, Duration::ZERO)?;
    host.observe(
        "p",
        "shared",
        &[exhausted(Utc::now() + chrono::Duration::days(3))],
    )?;
    let calls = AtomicUsize::new(0);
    let response = host
        .execute("p", "shared", "sonnet", CancellationToken::new(), || {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                Ok(ModelResponse {
                    text: "done".into(),
                    ..Default::default()
                })
            }
        })
        .await?;
    assert_eq!(response.text, "done");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[test]
fn appended_prior_observations_keep_chronological_changes_and_late_arrival_guard() -> Result<()> {
    let now = Utc::now();
    let store = Store::in_memory()?;
    let host = QuotaHost::new(store.clone(), Duration::ZERO)?;
    let mut current = observed(
        "primary",
        now + chrono::Duration::minutes(10),
        QuotaSource::ResponseHeaders,
    );
    current.observed_at = now.to_rfc3339();
    let mut prior = current.clone();
    prior.observed_at = (now - chrono::Duration::seconds(1)).to_rfc3339();
    prior.used_percent = 10.0;
    host.observe("p", "shared", &[current, prior.clone()])?;
    let history = store.quota_history()?;
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].used_percent, 10.0);
    assert_eq!(history[1].used_percent, 100.0);
    // A separate old completion still cannot overwrite the newer account state.
    host.observe("p", "shared", &[prior])?;
    assert_eq!(store.quota_history()?.len(), 2);
    assert_eq!(host.latest("shared")[0].used_percent, 100.0);
    Ok(())
}

struct ShortRejection {
    calls: AtomicUsize,
    publish: bool,
}
#[async_trait]
impl Provider for ShortRejection {
    async fn generate(
        &self,
        _: ModelRequest,
        on_text: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            if self.publish {
                on_text("already visible");
            }
            return Err(rejected(None));
        }
        Ok(ModelResponse {
            text: "finished".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn short_rejection_retries_inline_but_published_text_never_retries() -> Result<()> {
    for publish in [false, true] {
        let store = Store::in_memory()?;
        let session = store.create_session("short")?;
        let provider = Arc::new(ShortRejection {
            calls: AtomicUsize::new(0),
            publish,
        });
        let harness = harness(store.clone(), provider.clone(), Duration::from_secs(60))?;
        let run = harness
            .prompt(&session.id, "go", RunOptions::default())
            .await?;
        assert_eq!(run.status, if publish { "failed" } else { "completed" });
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            if publish { 1 } else { 2 }
        );
        assert!(store.pending_quota_waits()?.is_empty());
        let usage = store.request_usage()?;
        assert_eq!(usage.len(), if publish { 1 } else { 2 });
        assert!(usage.iter().all(|row| row.account == "shared"));
    }
    Ok(())
}

struct MeteredSuccess;

#[tokio::test]
async fn quota_wait_preserves_the_existing_short_retry_counter() -> Result<()> {
    let host = QuotaHost::new(Store::in_memory()?, Duration::from_secs(60))?;
    let calls = AtomicUsize::new(0);
    let error = host
        .execute("p", "shared", "m", CancellationToken::new(), || {
            let ordinal = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if ordinal == 2 {
                    let reset = Utc::now() + chrono::Duration::milliseconds(10);
                    return Err(ProviderError::new(
                        ProviderErrorKind::RateLimit {
                            resets_at: Some(reset.to_rfc3339()),
                        },
                        "rejected",
                    )
                    .with_quota(vec![exhausted(reset)])
                    .into());
                }
                Err(rejected(None))
            }
        })
        .await
        .expect_err("four short rejections exhaust the same retry counter");
    assert!(matches!(
        ProviderError::of(&error).unwrap().kind,
        ProviderErrorKind::RateLimit { .. }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    Ok(())
}

#[async_trait]
impl Provider for MeteredSuccess {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let mut reading = exhausted(Utc::now());
        reading.used_percent = 30.0;
        reading.resets_at = Some("2030-01-01T00:00:00Z".into());
        reading.source = QuotaSource::SdkEvent;
        Ok(ModelResponse {
            text: "done".into(),
            quota: vec![reading],
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn unchanged_meter_adds_no_normal_model_step_transaction() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("meter")?;
    let harness = harness(
        store.clone(),
        Arc::new(MeteredSuccess),
        Duration::from_secs(60),
    )?;
    assert_eq!(
        harness
            .prompt(&session.id, "warm", RunOptions::default())
            .await?
            .status,
        "completed"
    );
    let before = store.storage_stats()?;
    assert_eq!(
        harness
            .prompt(&session.id, "same", RunOptions::default())
            .await?
            .status,
        "completed"
    );
    let after = store.storage_stats()?;
    assert_eq!(
        after.chat_transaction_count - before.chat_transaction_count,
        2
    );
    assert_eq!(
        after.serving_transaction_count - before.serving_transaction_count,
        1
    );
    assert_eq!(
        after.catalog_transaction_count - before.catalog_transaction_count,
        0
    );
    assert_eq!(store.quota_history()?.len(), 1);
    println!(
        "hosted unchanged quota: chat_tx=2 serving_tx=1 catalog_tx=0 database_bytes={}->{} wal_bytes={}->{}",
        before.database_bytes, after.database_bytes, before.wal_bytes, after.wal_bytes
    );
    Ok(())
}

struct RejectedOnce {
    calls: AtomicUsize,
    reset: DateTime<Utc>,
    terminal: bool,
}
#[async_trait]
impl Provider for RejectedOnce {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            let mut error = if self.terminal {
                ProviderError::new(
                    ProviderErrorKind::Fatal,
                    "billing or spend limit reached: enforced_spend_limit_reached",
                )
            } else {
                ProviderError::new(
                    ProviderErrorKind::RateLimit {
                        resets_at: Some(self.reset.to_rfc3339()),
                    },
                    "request rejected",
                )
            };
            error.quota = vec![exhausted(self.reset)];
            return Err(error.into());
        }
        assert!(
            Utc::now() >= self.reset,
            "request dispatched before quota reset"
        );
        Ok(ModelResponse {
            text: "finished".into(),
            ..Default::default()
        })
    }
}

fn harness(store: Store, provider: Arc<dyn Provider>, max_wait: Duration) -> Result<Arc<Harness>> {
    let mut registry = ProviderRegistry::new("p", provider);
    registry.set_account("p", "shared");
    registry.set_usage_store(store.clone());
    registry.set_quota_host(Arc::new(QuotaHost::new(store.clone(), max_wait)?));
    Ok(Arc::new(Harness::new(
        store,
        Arc::new(registry),
        ToolRegistry::new(),
        HarnessConfig {
            model: "p/model".into(),
            ..Default::default()
        },
    )))
}

#[tokio::test]
async fn in_process_wait_keeps_run_and_one_round_budget_and_publishes_event() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("quota")?;
    let provider = Arc::new(RejectedOnce {
        calls: AtomicUsize::new(0),
        reset: Utc::now() + chrono::Duration::milliseconds(150),
        terminal: false,
    });
    let harness = harness(store.clone(), provider.clone(), Duration::from_secs(10))?;
    let mut events = harness.subscribe();
    let started = harness.start(
        &session.id,
        "go",
        RunOptions {
            max_rounds: Some(1),
            ..Default::default()
        },
    )?;
    let waited = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = events.recv().await?;
            if let bubblepaw_core::EventKind::RunWaitingForQuota { wait } = event.kind {
                break Ok::<_, anyhow::Error>(wait);
            }
        }
    })
    .await??;
    assert_eq!(waited.run_id, started.id);
    assert_eq!(waited.account, "shared");
    assert_eq!(store.run(&started.id)?.status, "waiting_for_quota");
    let done = harness.wait_for_idle(&session.id).await?;
    assert_eq!(done.id, started.id);
    assert_eq!(done.status, "completed");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert!(store.pending_quota_waits()?.is_empty());
    Ok(())
}

#[tokio::test]
async fn max_wait_and_spend_cap_fail_without_retry_or_marker() -> Result<()> {
    for terminal in [false, true] {
        let store = Store::in_memory()?;
        let session = store.create_session("quota")?;
        let reset = Utc::now() + chrono::Duration::hours(7);
        let provider = Arc::new(RejectedOnce {
            calls: AtomicUsize::new(0),
            reset,
            terminal,
        });
        let harness = harness(store.clone(), provider.clone(), Duration::from_secs(21600))?;
        let run = harness
            .prompt(&session.id, "go", RunOptions::default())
            .await?;
        assert_eq!(run.status, "failed");
        let message = run.error.unwrap();
        assert!(if terminal {
            message.contains("enforced_spend_limit_reached")
        } else {
            message.contains("quota exhausted until")
                && message.contains("shared")
                && message.contains("five_hour")
        });
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert!(store.pending_quota_waits()?.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn cancellation_clears_a_live_wait_without_second_dispatch() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("quota")?;
    let provider = Arc::new(RejectedOnce {
        calls: AtomicUsize::new(0),
        reset: Utc::now() + chrono::Duration::seconds(30),
        terminal: false,
    });
    let harness = harness(store.clone(), provider.clone(), Duration::from_secs(60))?;
    harness.start(&session.id, "go", RunOptions::default())?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while store.quota_wait(&session.id)?.is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    assert!(harness.cancel(&session.id));
    assert_eq!(
        harness.wait_for_idle(&session.id).await?.status,
        "cancelled"
    );
    assert!(store.pending_quota_waits()?.is_empty());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[test]
fn process_restart_resumes_at_reset_in_a_new_run() -> Result<()> {
    for resume_phase in ["resume", "due_once", "explicit"] {
        let root = tempfile::tempdir()?;
        for phase in ["park", resume_phase] {
            let output = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--ignored",
                    "--exact",
                    "restart_process_fixture",
                    "--nocapture",
                ])
                .env("BUBBLEPAW_QUOTA_TEST_DIR", root.path())
                .env("BUBBLEPAW_QUOTA_TEST_PHASE", phase)
                .output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let store = Store::open_data_dir(root.path().join("data"))?;
        let session = store.list_sessions()?.pop().unwrap();
        let runs = store.runs(&session.id)?;
        assert_eq!(runs.len(), 2);
        assert_ne!(runs[0].id, runs[1].id);
        assert_eq!(runs[0].status, "interrupted");
        assert_eq!(runs[1].status, "completed");
        assert!(store.pending_quota_waits()?.is_empty());
    }
    Ok(())
}

#[tokio::test]
#[ignore = "invoked as a real subprocess by the restart test"]
async fn restart_process_fixture() -> Result<()> {
    let root = std::path::PathBuf::from(std::env::var_os("BUBBLEPAW_QUOTA_TEST_DIR").unwrap());
    let store = Store::open_data_dir(root.join("data"))?;
    if std::env::var("BUBBLEPAW_QUOTA_TEST_PHASE")? == "park" {
        let session = store.create_session("restart")?;
        let provider = Arc::new(RejectedOnce {
            calls: AtomicUsize::new(0),
            reset: Utc::now() + chrono::Duration::seconds(2),
            terminal: false,
        });
        let harness = harness(store.clone(), provider, Duration::from_secs(60))?;
        harness.start(
            &session.id,
            "go",
            RunOptions {
                max_rounds: Some(1),
                ..Default::default()
            },
        )?;
        tokio::time::timeout(Duration::from_secs(2), async {
            while store.quota_wait(&session.id)?.is_none() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        std::process::exit(0);
    }
    let wait = store.pending_quota_waits()?.pop().unwrap();
    let reset = DateTime::parse_from_rfc3339(&wait.resume_at)?.with_timezone(&Utc);
    let provider = Arc::new(RejectedOnce {
        calls: AtomicUsize::new(1),
        reset,
        terminal: false,
    });
    let harness = harness(store.clone(), provider.clone(), Duration::from_secs(60))?;
    let phase = std::env::var("BUBBLEPAW_QUOTA_TEST_PHASE")?;
    if phase != "resume" {
        tokio::time::sleep(
            reset
                .signed_duration_since(Utc::now())
                .to_std()
                .unwrap_or_default(),
        )
        .await;
        let skip = (phase == "explicit").then_some(wait.session_id.as_str());
        let count = bubblepaw_runner::quota_recovery::resume_due_once(
            &harness,
            &root_resume(harness.clone()),
            skip,
        )
        .await?;
        if phase == "explicit" {
            assert_eq!(count, 0);
            harness
                .resume(&wait.session_id, RunOptions::default())
                .await?;
        } else {
            assert_eq!(count, 1);
        }
        assert_eq!(store.runs(&wait.session_id)?.len(), 2);
        return Ok(());
    }
    let recovery = QuotaRecovery::start(harness.clone(), root_resume(harness.clone()))?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while store.runs(&wait.session_id)?.len() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let run = harness.wait_for_idle(&wait.session_id).await?;
    assert_eq!(run.status, "completed");
    assert_ne!(run.id, wait.run_id);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    recovery.shutdown().await;
    Ok(())
}
