use anyhow::{Result, bail};
use async_trait::async_trait;
use bubblepaw_core::*;
use bubblepaw_runner::trace::{JsonlTrace, TraceConfig, read_report};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[path = "tracing/support.rs"]
mod support;
use support::run;

#[tokio::test]
async fn default_is_disabled_and_does_not_create_trace_files() -> Result<()> {
    assert!(HarnessConfig::default().observation.is_none());
    let (directory, result) = run(None, false).await?;
    assert_eq!(result.status, "completed");
    assert!(!directory.path().join("trace.jsonl").exists());
    Ok(())
}
#[tokio::test]
async fn metadata_correlates_real_requests_nested_calls_errors_usage_and_repeats() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("trace.jsonl");
    let trace = JsonlTrace::open(TraceConfig {
        path: path.clone(),
        label: "revision-a".into(),
        ..Default::default()
    })?;
    let (_, result) = run(Some(&trace), false).await?;
    trace.flush(Duration::from_secs(2))?;
    trace.finish(Duration::from_secs(2))?;
    assert_eq!(result.status, "completed");
    let raw = std::fs::read_to_string(&path)?;
    for private in [
        "private-prompt",
        "private-command",
        "private-value",
        "done-private-text",
        "provider-private-state",
        "tool-private-credential",
        "input-private-credential",
        "private-stdout-data",
        "private-stderr-data",
    ] {
        assert!(!raw.contains(private), "leaked {private}");
    }
    let records: Vec<Value> = raw
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    let nested: Vec<_> = records
        .iter()
        .map(|e| &e["record"])
        .filter(|r| r["kind"] == "tool_started" && r["metrics"]["nested"] == true)
        .collect();
    assert_eq!(nested.len(), 4);
    for call in nested {
        assert_eq!(call["execution_id"], "cell-42");
        assert_eq!(call["generation"], 7);
        assert!(call["model_request_id"].is_string());
        assert_eq!(call["run_id"], result.id);
        assert!(call.get("payload").is_none());
    }
    let report = read_report(path)?;
    assert_eq!(report.model_calls, 2);
    assert_eq!(report.input_tokens, 15);
    assert_eq!(report.output_tokens, 9);
    assert_eq!(report.tool_errors, 1);
    assert_eq!(report.process_observations, 3);
    assert_eq!(report.process_nonzero_exits, 3);
    assert_eq!(report.process_statuses["exited"], 3);
    let process_metrics: Vec<_> = records
        .iter()
        .map(|e| &e["record"]["metrics"]["process"])
        .filter(|m| m.is_object())
        .collect();
    assert_eq!(process_metrics.len(), 3);
    for process in process_metrics {
        assert_eq!(process["exit_code"], 9);
        assert_eq!(process["stdout"]["total_bytes"], 101);
        assert_eq!(process["stdout"]["gap_count"], 2);
        assert_eq!(process["stdout"]["capture_error"], true);
    }
    assert_eq!(report.tool_calls["exec"], 4);
    assert_eq!(report.calls_per_execution["cell-42"], 4);
    assert_eq!(report.command_kinds["raw_script"], 2);
    assert_eq!(report.command_kinds["template"], 1);
    assert_eq!(
        report.repeated_inputs.values().copied().collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(report.incomplete_calls, 0);
    assert_eq!(report.execution_errors, 1);
    assert_eq!(report.script_prefixes["echo"], 2);
    assert_eq!(report.program_attempts["echo"], 1);
    Ok(())
}

struct WaitingProvider(Arc<tokio::sync::Notify>);
#[async_trait]
impl Provider for WaitingProvider {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        self.0.notify_one();
        cancellation.cancelled().await;
        bail!("cancelled")
    }
}
#[tokio::test]
async fn cancellation_and_steering_are_correlated_without_recording_metadata_text() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("trace.jsonl");
    let trace = JsonlTrace::open(TraceConfig {
        path: path.clone(),
        ..Default::default()
    })?;
    let store = Store::open(directory.path().join("archive.sqlite"))?;
    let session = store.create_session("cancel")?;
    let entered = Arc::new(tokio::sync::Notify::new());
    let harness = Harness::new(
        store,
        Arc::new(WaitingProvider(entered.clone())),
        ToolRegistry::default(),
        HarnessConfig {
            observation: Some(trace.observation()),
            ..Default::default()
        },
    );
    harness.start(&session.id, "start", RunOptions::default())?;
    tokio::time::timeout(Duration::from_secs(2), entered.notified()).await?;
    harness.steer(&session.id, "private-steering-text")?;
    harness.followup(&session.id, "private-followup-text")?;
    assert!(harness.cancel(&session.id));
    let run =
        tokio::time::timeout(Duration::from_secs(2), harness.wait_for_idle(&session.id)).await??;
    assert_eq!(run.status, "cancelled");
    trace.finish(Duration::from_secs(2))?;
    let report = read_report(&path)?;
    assert_eq!(report.model_calls, 1);
    assert_eq!(report.model_errors, 1);
    assert_eq!(report.cancel_intents, 1);
    assert_eq!(report.accepted_inputs["steer"], 1);
    assert_eq!(report.accepted_inputs["followup"], 1);
    let raw = std::fs::read_to_string(path)?;
    assert!(!raw.contains("private-steering-text"));
    assert!(!raw.contains("private-followup-text"));
    Ok(())
}

#[test]
fn rotation_failures_are_observable_and_never_redirect_records_to_a_temporary_file() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("trace.jsonl");
    // A directory in the rotation slot reproduces a Windows-compatible rename failure.
    std::fs::create_dir(directory.path().join("trace.jsonl.1"))?;
    let trace = JsonlTrace::open(TraceConfig {
        path,
        max_record_bytes: 1024,
        max_payload_bytes: 512,
        max_file_bytes: 1024,
        retained_files: 2,
        ..Default::default()
    })?;
    for _ in 0..10 {
        trace.runtime_info(json!({"test":"forced rotation failure"}));
    }
    assert!(trace.finish(Duration::from_secs(2)).is_err());
    assert!(trace.status().write_errors > 0);
    assert!(trace.status().dropped > 0);
    assert!(trace.status().last_error.is_some());
    Ok(())
}

#[test]
fn losses_become_visible_in_live_reports_without_an_explicit_flush() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("trace.jsonl");
    let trace = JsonlTrace::open(TraceConfig {
        path: path.clone(),
        max_record_bytes: 1024,
        max_payload_bytes: 512,
        ..Default::default()
    })?;
    trace.runtime_info(json!({"oversized":"x".repeat(4096)}));
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if read_report(&path)?.dropped_records == 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "loss status never reached live report"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let written = trace.status().written;
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        trace.status().written,
        written,
        "idle writer created redundant status records"
    );
    trace.finish(Duration::from_secs(2))?;
    Ok(())
}

#[tokio::test]
async fn contents_previews_respect_byte_caps_and_explicitly_mark_truncation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("trace.jsonl");
    let trace = JsonlTrace::open(TraceConfig {
        path: path.clone(),
        mode: ObservationMode::Contents,
        max_payload_bytes: 128,
        ..Default::default()
    })?;
    run(Some(&trace), false).await?;
    trace.finish(Duration::from_secs(2))?;
    let raw = std::fs::read_to_string(path)?;
    let records: Vec<Value> = raw
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    assert!(
        records
            .iter()
            .any(|r| r["record"]["payload_truncated"] == true)
    );
    for record in records {
        if let Some(payload) = record["record"]["payload"].as_str() {
            assert!(payload.len() <= 128);
        }
    }
    Ok(())
}
#[tokio::test]
async fn contents_captures_prompts_but_excludes_continuation_state_and_structured_credentials()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("trace.jsonl");
    let trace = JsonlTrace::open(TraceConfig {
        path: path.clone(),
        mode: ObservationMode::Contents,
        ..Default::default()
    })?;
    run(Some(&trace), false).await?;
    trace.finish(Duration::from_secs(2))?;
    let raw = std::fs::read_to_string(path)?;
    assert!(raw.contains("private-prompt"));
    assert!(raw.contains("private-command"));
    for secret in [
        "provider-private-state",
        "tool-private-credential",
        "input-private-credential",
    ] {
        assert!(!raw.contains(secret), "leaked {secret}");
    }
    Ok(())
}
#[tokio::test]
async fn provider_failures_are_counted_without_claiming_token_usage() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("trace.jsonl");
    let trace = JsonlTrace::open(TraceConfig {
        path: path.clone(),
        ..Default::default()
    })?;
    let (_, run) = run(Some(&trace), true).await?;
    trace.finish(Duration::from_secs(2))?;
    assert_eq!(run.status, "failed");
    let report = read_report(&path)?;
    assert_eq!(report.model_calls, 1);
    assert_eq!(report.model_errors, 1);
    assert_eq!(report.model_calls_without_usage, 1);
    assert_eq!(report.input_tokens, 0);
    assert!(!std::fs::read_to_string(path)?.contains("provider-private-failure"));
    Ok(())
}
#[test]
fn retention_and_record_caps_keep_valid_json_and_allow_explicit_finish() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("trace.jsonl");
    let trace = JsonlTrace::open(TraceConfig {
        path: path.clone(),
        max_record_bytes: 1024,
        max_payload_bytes: 512,
        max_file_bytes: 2048,
        retained_files: 2,
        queue_capacity: 256,
        ..Default::default()
    })?;
    for _ in 0..25 {
        trace.runtime_info(json!({"test":"bounded records"}));
    }
    trace.finish(Duration::from_secs(2))?;
    let files: Vec<_> = std::fs::read_dir(directory.path())?.collect::<std::io::Result<_>>()?;
    assert!(files.len() <= 2);
    for file in files {
        assert!(file.metadata()?.len() <= 2048);
        for line in std::fs::read_to_string(file.path())?.lines() {
            assert!(line.len() < 1024);
            let _: Value = serde_json::from_str(line)?;
        }
    }
    assert!(read_report(path)?.records > 0);
    Ok(())
}

#[test]
fn trace_report_aggregates_cache_accounting_per_provider_model() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("trace.jsonl");
    let line = |sequence: u64, kind: &str, metrics: Value| {
        json!({"schema_version":1,"trace_id":"t","sequence":sequence,"label":"","record":{"created_at":"2026-10-19T00:00:00Z","kind":kind,"session_id":"s","run_id":"r","metrics":metrics,"payload_truncated":false}}).to_string()
    };
    let lines = [
        // Main loop: an undeclared break.
        line(
            1,
            "model_finished",
            json!({"usage":{"input_tokens":1000},"cache_group":{"provider":"opencode-go","model":"deepseek","kind":"main"},"expected_cacheable_tokens":960,"measured_cache_read_tokens":896,"unexplained_miss_tokens":64,"prefix_break":"undeclared"}),
        ),
        line(
            2,
            "cache_bust",
            json!({"provider":"opencode-go","model":"deepseek","declared":false,"reason":"undeclared"}),
        ),
        // Empty provider: keyed by the model name; null numbers count as zero.
        line(
            3,
            "model_finished",
            json!({"usage":{"input_tokens":500},"cache_group":{"provider":"","model":"local","kind":"decider"},"expected_cacheable_tokens":null,"measured_cache_read_tokens":0,"unexplained_miss_tokens":null,"prefix_break":null}),
        ),
        // Episode call, then a declared break on the same route.
        line(
            4,
            "runtime_info",
            json!({"event":"episode_model_call","cache_group":{"provider":"opencode-go","model":"deepseek","kind":"summary_fork"},"expected_cacheable_tokens":128,"measured_cache_read_tokens":128,"unexplained_miss_tokens":0}),
        ),
        line(
            5,
            "cache_bust",
            json!({"provider":"opencode-go","model":"deepseek","declared":true,"reason":"lcm_apply"}),
        ),
    ];
    std::fs::write(&path, lines.join("\n") + "\n")?;
    let report = read_report(&path)?;
    let main = &report.cache["opencode-go/deepseek"];
    assert_eq!(main.requests, 2);
    assert_eq!(main.input_tokens, 1000);
    assert_eq!(main.measured_cache_read_tokens, 1024);
    assert_eq!(main.expected_cacheable_tokens, 1088);
    assert_eq!(main.unexplained_miss_tokens, 64);
    assert_eq!((main.declared_busts, main.undeclared_busts), (1, 1));
    let local = &report.cache["local"];
    assert_eq!((local.requests, local.input_tokens), (1, 500));
    assert_eq!(
        (
            local.expected_cacheable_tokens,
            local.unexplained_miss_tokens
        ),
        (0, 0)
    );
    assert_eq!(report.cache.len(), 2);
    Ok(())
}
