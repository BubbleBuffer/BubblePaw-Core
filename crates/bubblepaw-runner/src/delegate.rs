//! `bubblepaw delegate`: one assignment through the ordinary harness, accepted only by
//! harness-run checks, with an outcome ledger row per attempt. Delegation policy lives
//! here in the runner; the engine only runs turns.
pub mod assignment;
pub mod checks;
pub mod diff;
pub mod ledger;
pub mod metrics;
pub mod pricing;
mod prompts;
pub mod read_first;
pub mod report;
pub mod routing;

use anyhow::Result;
use assignment::Assignment;
use bubblepaw_core::{CancellationToken, Harness, HarnessEvent, RunOptions, Store};
use checks::CheckResult;
use ledger::{AttemptRecord, Ledger};
use metrics::{Collector, Observed};
use pricing::{PriceTable, TokenClasses};
use prompts::{first_prompt, rework_prompt};
use report::FinalReport;
use routing::RouteChoice;
use serde::Serialize;
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

/// Bytes of the worker summary kept in the result record.
pub const SUMMARY_BYTES: usize = 2000;

pub type EventSink = Arc<dyn Fn(&HarnessEvent) + Send + Sync>;
/// Called with the new session id, right after the session is created.
pub type SessionHook = Arc<dyn Fn(&str) + Send + Sync>;

pub struct DelegateRun {
    pub assignment: Assignment,
    pub route: RouteChoice,
    pub workspace: PathBuf,
    /// Session title for the worker.
    pub title: String,
    pub strategy: bubblepaw_core::ContextStrategy,
    /// Called for each event of the worker session, for JSONL output.
    pub events: Option<EventSink>,
    /// The observation sink given to the harness; None leaves call metrics empty.
    pub collector: Option<Arc<Collector>>,
}

/// The compact record printed last for the dispatcher.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct DelegationResult {
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// accepted, failed or unchecked.
    pub status: String,
    /// Ledger id of the last attempt; a follow-up names it as `parent`.
    pub ledger_id: String,
    pub delegation_id: String,
    pub session_id: String,
    pub model: String,
    pub class: Option<String>,
    pub route_reason: String,
    pub attempts: u32,
    /// Verified lines from the harness's own check runs on the last attempt.
    pub checks: Vec<String>,
    /// Failing checks with their bounded tails, for a failed resolution.
    pub failing: Vec<CheckResult>,
    pub changed_files: Vec<String>,
    pub diff_stat: Option<diff::DiffStat>,
    pub report_status: Option<String>,
    pub summary: String,
    pub open_questions: Vec<String>,
    pub false_greens: usize,
    pub cost_usd: Option<f64>,
    pub price_table: String,
    pub error: Option<String>,
}

fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Rough token count of the assignment, for dispatch features.
fn approx_tokens(text: &str) -> u64 {
    tiktoken_rs::o200k_base_singleton()
        .encode_ordinary(text)
        .len() as u64
}

async fn repo_name(workspace: &std::path::Path) -> Option<String> {
    let output = crate::process_output::output(
        tokio::process::Command::new("git")
            .current_dir(workspace)
            .args(["rev-parse", "--show-toplevel"]),
    )
    .await
    .ok()?;
    output.status.success().then(|| {
        let top = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        std::path::Path::new(&top)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or(top)
    })
}

/// Run the assignment: a fresh session, checks, and up to `rework` resumes.
pub async fn run(
    harness: Arc<Harness>,
    store: &Store,
    ledger: &mut Ledger,
    request: DelegateRun,
) -> Result<DelegationResult> {
    run_with_control(
        harness,
        store,
        ledger,
        request,
        CancellationToken::default(),
        None,
    )
    .await
}

/// [`run`] with a cancellation token and a callback for the new session id, so a host can
/// scope event forwarding and cancellation. Cancelling stops the model run of this session
/// (`Harness::cancel`) or the running checks and their process trees; the attempt is recorded
/// once as failed with a `cancelled` run status, and no rework follows.
pub async fn run_with_control(
    harness: Arc<Harness>,
    store: &Store,
    ledger: &mut Ledger,
    request: DelegateRun,
    cancellation: CancellationToken,
    on_session: Option<SessionHook>,
) -> Result<DelegationResult> {
    run_controlled(
        harness,
        store,
        ledger,
        request,
        DelegateControl {
            cancellation,
            on_session,
            ..DelegateControl::default()
        },
    )
    .await
}

pub type SessionStarted = Arc<dyn Fn(&str) + Send + Sync>;

/// Request-owned execution settings. Omitted values preserve the library API defaults.
#[derive(Default)]
pub struct DelegateControl {
    pub cancellation: CancellationToken,
    pub on_session: Option<SessionStarted>,
    pub storage_policy: Option<bubblepaw_core::RootStoragePolicy>,
    pub environment: Option<Vec<(String, String)>>,
}

pub async fn run_controlled(
    harness: Arc<Harness>,
    store: &Store,
    ledger: &mut Ledger,
    request: DelegateRun,
    control: DelegateControl,
) -> Result<DelegationResult> {
    let DelegateControl {
        cancellation,
        on_session,
        storage_policy,
        environment,
    } = control;
    let DelegateRun {
        assignment,
        route,
        workspace,
        title,
        strategy,
        events,
        collector,
    } = request;
    // Before the session, the ledger and any model call: a bad entry rejects the assignment.
    let read_first_text = read_first::section(&assignment.read_first, &workspace)?;
    if let Some((parent, _)) = &assignment.parent {
        anyhow::ensure!(
            ledger.attempt_exists(parent)?,
            "parent ledger id {parent} is not in the delegation ledger"
        );
    }
    let prices = PriceTable::embedded();
    let delegation_id = uuid::Uuid::new_v4().to_string();
    let session = match storage_policy {
        Some(policy) => store.create_session_with_policy(title, strategy, &workspace, policy)?,
        None => store.create_session_with_strategy_and_workspace(title, strategy, &workspace)?,
    };
    if let Some(on_session) = &on_session {
        on_session(&session.id);
    }
    // The assignment length counts the `read_first` contents too; the hash does not.
    let assignment_tokens = approx_tokens(&assignment.body) + approx_tokens(&read_first_text);
    let repo = repo_name(&workspace).await;
    let workspace_text = workspace.display().to_string();
    let schema = report::schema();
    let start_tree = diff::snapshot(&workspace).await;
    let mut prompt = first_prompt(&assignment, &read_first_text);
    let mut rework_index = 0u32;
    let mut total_cost = Some(0.0);
    loop {
        let started_at = chrono::Utc::now();
        let wall = Instant::now();
        let seen_messages = store.messages(&session.id)?.len();
        let seen_episodes = store.context_episodes(&session.id).ok().map(|e| e.len());
        let observed = Arc::new(Mutex::new(Observed::default()));
        let mut receiver = harness.subscribe();
        let target = session.id.clone();
        let sink = events.clone();
        let recorder = observed.clone();
        let mut watcher = tokio::spawn(async move {
            loop {
                let event = match receiver.recv().await {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        recorder.lock().unwrap().lagged += missed;
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                if event.session_id != target {
                    continue;
                }
                if let Some(sink) = &sink {
                    sink(&event);
                }
                recorder.lock().unwrap().push(chrono::Utc::now(), &event);
                if matches!(event.kind, bubblepaw_core::EventKind::SessionSettled {}) {
                    break;
                }
            }
        });
        let options = RunOptions {
            output_schema: Some(schema.clone()),
            ..Default::default()
        };
        // Cancelling asks only this session's run to stop, then waits for its terminal result.
        // The prompt branch is polled first, so the run is admitted before a cancel can arrive.
        let run = if cancellation.is_cancelled() {
            None
        } else {
            let mut prompt_run =
                std::pin::pin!(harness.prompt(&session.id, prompt.clone(), options));
            let finished = tokio::select! {
                biased;
                run = &mut prompt_run => Some(run),
                () = cancellation.cancelled() => None,
            };
            match finished {
                Some(run) => Some(run),
                None => {
                    harness.cancel(&session.id);
                    Some(prompt_run.await)
                }
            }
        };
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), &mut watcher).await;
        watcher.abort();
        let model_phase = wall.elapsed();
        let (run_status, run_text, run_error, usage) = match &run {
            Some(Ok(run)) => (
                run.status.clone(),
                run.text.clone(),
                run.error.clone(),
                run.usage.clone(),
            ),
            Some(Err(error)) => ("error".to_owned(), None, Some(format!("{error:#}")), None),
            None => (
                "cancelled".to_owned(),
                None,
                Some("cancelled before the model run".to_owned()),
                None,
            ),
        };
        // A run that did not complete is a failed resolution; always name the cause.
        let run_error = run_error.or_else(|| {
            (run_status != "completed")
                .then(|| format!("worker run ended with status {run_status}"))
        });
        let report = (run_status == "completed")
            .then(|| run_text.as_deref().and_then(|t| FinalReport::parse(t).ok()))
            .flatten();
        // Checks run only after a completed turn; an errored run is a failed resolution.
        let check_started = Instant::now();
        let results = if run_status == "completed" {
            checks::run_all_with_environment(
                &workspace,
                &assignment.checks,
                &cancellation,
                environment.as_deref(),
            )
            .await
        } else {
            Vec::new()
        };
        let check_ms = check_started.elapsed().as_millis() as u64;
        let all_passed = results.iter().all(CheckResult::passed);
        // Cancelled during the checks: a check was cut short, or one failed as the stop came.
        // The attempt's cause is then the cancellation, and it is never reworked.
        let checks_cancelled = run_status == "completed"
            && cancellation.is_cancelled()
            && (results.len() < assignment.checks.len() || !all_passed);
        let (run_status, run_error) = if checks_cancelled {
            (
                "cancelled".to_owned(),
                Some("cancelled during the acceptance checks".to_owned()),
            )
        } else {
            (run_status, run_error)
        };
        let messages = store.messages(&session.id)?;
        let new_messages = &messages[seen_messages.min(messages.len())..];
        let execs = metrics::recorded_execs(&observed.lock().unwrap());
        let false_greens = report::false_greens(report.as_ref(), &results, &execs);
        let end_tree = diff::snapshot(&workspace).await;
        let diff_stat = match (&start_tree, &end_tree) {
            (Some(a), Some(b)) => diff::between(&workspace, a, b).await,
            _ => None,
        };
        let can_rework = run_status == "completed" && rework_index < assignment.rework;
        let final_status = if run_status != "completed" {
            "failed"
        } else if assignment.checks.is_empty() {
            "unchecked"
        } else if checks_cancelled {
            "failed"
        } else if all_passed {
            "accepted"
        } else if can_rework {
            "rework"
        } else {
            "failed"
        };
        let tokens = usage.as_ref().map(TokenClasses::from).unwrap_or_default();
        let records = collector
            .as_ref()
            .map(|c| c.take(&session.id))
            .unwrap_or_default();
        let new_episodes = seen_episodes
            .zip(store.context_episodes(&session.id).ok().map(|e| e.len()))
            .map(|(before, after)| after.saturating_sub(before) as u64);
        let unknown_effects = store
            .pending_effects(&session.id)
            .map(|effects| {
                effects
                    .iter()
                    .filter(|e| {
                        e.get("state").and_then(serde_json::Value::as_str) == Some("unknown")
                    })
                    .count() as u64
            })
            .unwrap_or(0);
        let collected = metrics::collect(metrics::Input {
            started_at,
            messages: new_messages,
            observed: &observed.lock().unwrap(),
            records: &records,
            dropped_records: collector.as_ref().map_or(0, |c| c.dropped()),
            checks: &results,
            assignment_checks: &assignment.checks,
            report: report.as_ref(),
            diff: diff_stat.as_ref(),
            model: &route.model,
            model_phase,
            attempt_tokens: &tokens,
            new_episodes,
            unknown_effects,
        });
        // Price per call when every call reported usage (period by call time), else the
        // run total at the attempt's start time.
        let cost =
            if !collected.calls.is_empty() && collected.calls.iter().all(|c| c.tokens.is_some()) {
                collected.calls.iter().try_fold(0.0, |sum, call| {
                    Some(sum + prices.cost(&call.model, call.tokens.as_ref()?, call.at)?)
                })
            } else {
                prices.cost(&route.model, &tokens, Some(started_at))
            };
        total_cost = total_cost.zip(cost).map(|(a, b)| a + b);
        let attempt_id = uuid::Uuid::new_v4().to_string();
        let record = AttemptRecord {
            id: attempt_id.clone(),
            delegation_id: delegation_id.clone(),
            parent_id: assignment.parent.as_ref().map(|(p, _)| p.clone()),
            started_at: started_at.to_rfc3339(),
            ended_at: chrono::Utc::now().to_rfc3339(),
            assignment_hash: assignment.hash.clone(),
            category: assignment.category.clone(),
            tags: assignment.tags.clone(),
            workspace: workspace_text.clone(),
            repo: repo.clone(),
            assignment_tokens,
            check_count: assignment.checks.len(),
            route_class: route.class.clone(),
            model: route.model.clone(),
            route_reason: route.reason.clone(),
            period: prices.period(&route.model, Some(started_at)).to_owned(),
            rework_index,
            session_id: session.id.clone(),
            run_status: run_status.clone(),
            report_status: report.as_ref().map(|r| r.status.clone()),
            final_status: final_status.to_owned(),
            accepted: final_status == "accepted",
            checks: json!(results),
            false_greens: false_greens.len(),
            tokens,
            cost_usd: cost,
            price_table: prices.checked.clone(),
            wall_ms: wall.elapsed().as_millis() as u64,
            check_ms,
            metrics: collected.attempt,
            calls: collected.calls,
        };
        ledger.record(&record)?;
        if final_status == "rework" {
            let failing: Vec<_> = results.iter().filter(|c| !c.passed()).collect();
            prompt = rework_prompt(&failing);
            rework_index += 1;
            continue;
        }
        let failing = results.iter().filter(|c| !c.passed()).cloned().collect();
        return Ok(DelegationResult {
            kind: "delegation",
            status: if final_status == "accepted" || final_status == "unchecked" {
                final_status.to_owned()
            } else {
                "failed".to_owned()
            },
            ledger_id: attempt_id,
            delegation_id,
            session_id: session.id.clone(),
            model: route.model.clone(),
            class: route.class.clone(),
            route_reason: route.reason.clone(),
            attempts: rework_index + 1,
            checks: results.iter().map(CheckResult::line).collect(),
            failing,
            changed_files: diff_stat
                .as_ref()
                .map(|d| d.files.clone())
                .unwrap_or_default(),
            diff_stat,
            report_status: report.as_ref().map(|r| r.status.clone()),
            summary: report
                .as_ref()
                .map(|r| bounded(&r.summary, SUMMARY_BYTES))
                .or_else(|| run_text.as_deref().map(|t| bounded(t, SUMMARY_BYTES)))
                .unwrap_or_default(),
            open_questions: report.map(|r| r.open_questions).unwrap_or_default(),
            false_greens: false_greens.len(),
            cost_usd: total_cost,
            price_table: prices.checked.clone(),
            error: run_error,
        });
    }
}
