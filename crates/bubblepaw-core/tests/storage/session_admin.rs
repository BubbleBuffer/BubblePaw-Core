//! Renaming and deleting sessions, and the typed busy error.
use anyhow::{Result, bail};
use async_trait::async_trait;
use bubblepaw_core::{
    Approval, ApprovalMode, ApprovalPolicy, CancellationToken, Decision, EventKind, Harness,
    HarnessConfig, ModelRequest, ModelResponse, Provider, Role, RunOptions, SessionBusy, Store,
    TextCallback, Tool, ToolCall, ToolContext, ToolDefinition, ToolRegistry, ToolResult,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Calls `effect` once for a user message "call", blocks for "block", else answers.
#[derive(Default)]
struct Model {
    closed: AtomicUsize,
}
#[async_trait]
impl Provider for Model {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let last = request.messages.last().expect("a message");
        if last.role == Role::User && last.content == "block" {
            cancellation.cancelled().await;
            bail!("cancelled");
        }
        if last.role == Role::User && last.content == "call" {
            return Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "effect".into(),
                    arguments: json!({}),
                }],
                ..Default::default()
            });
        }
        Ok(ModelResponse {
            text: "done".into(),
            ..Default::default()
        })
    }
    async fn close_session(&self, _: &str) -> Result<()> {
        self.closed.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

struct Effect;
#[async_trait]
impl Tool for Effect {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "effect".into(),
            description: "needs approval".into(),
            input_schema: json!({"type":"object"}),
            read_only: false,
            approval: Approval::Confirm,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        Ok(ToolResult::text("effect done"))
    }
}

fn harness(store: &Store, model: Arc<Model>) -> Result<Harness> {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Effect))?;
    Ok(Harness::new(
        store.clone(),
        model,
        tools,
        HarnessConfig {
            approval: ApprovalPolicy {
                confirm: ApprovalMode::Ask,
                ..Default::default()
            },
            ..Default::default()
        },
    ))
}

/// Approve every request for the session, so approvals and rules are stored.
fn approve_all(harness: &Harness) -> tokio::task::JoinHandle<()> {
    let mut events = harness.subscribe();
    let harness = harness.clone();
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            if let EventKind::ApprovalRequested { approval } = event.kind {
                let _ = harness.decide(&approval.id, Decision::ApproveForSession);
            }
        }
    })
}

fn count(path: &std::path::Path, table: &str, session: &str) -> Result<i64> {
    let column = if table == "sessions" {
        "id"
    } else {
        "session_id"
    };
    let directory = Store::legacy_data_dir(path);
    let physical = if matches!(table, "events" | "runs" | "approvals" | "inputs") {
        directory.join("serving.db")
    } else {
        directory.join("chats").join(format!("{session}.db"))
    };
    if !physical.exists() {
        return Ok(0);
    }
    let conn = rusqlite::Connection::open(physical)?;
    Ok(conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE {column}=?1"),
        [session],
        |r| r.get(0),
    )?)
}

#[test]
fn rename_changes_title_and_rejects_unknown_sessions() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("Old")?;
    let renamed = store.rename_session(&session.id, "New")?;
    assert_eq!(renamed.title, "New");
    assert_eq!(store.session(&session.id)?.title, "New");
    assert!(store.rename_session("missing", "x").is_err());
    assert!(store.delete_session("missing").is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_removes_every_record_and_keeps_the_sequence() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("archive.sqlite");
    let model = Arc::new(Model::default());
    let (doomed, kept, last) = {
        let store = Store::open(&path)?;
        let harness = harness(&store, model.clone())?;
        let approver = approve_all(&harness);
        let kept = store.create_session("kept")?;
        let doomed = store.create_session("doomed")?;
        assert_eq!(
            harness
                .prompt(&kept.id, "call", RunOptions::default())
                .await?
                .status,
            "completed"
        );
        // The doomed session's events are the newest ones.
        assert_eq!(
            harness
                .prompt(&doomed.id, "call", RunOptions::default())
                .await?
                .status,
            "completed"
        );
        store.flush_serving()?;
        for table in [
            "messages",
            "runs",
            "inputs",
            "effects",
            "events",
            "approvals",
            "approval_rules",
        ] {
            assert!(count(&path, table, &doomed.id)? > 0, "{table} has rows");
        }
        let last = harness.last_sequence();
        harness.delete_session(&doomed.id).await?;
        assert_eq!(model.closed.load(Ordering::Relaxed), 1);
        assert!(store.session(&doomed.id).is_err());
        assert!(store.list_sessions()?.iter().all(|s| s.id != doomed.id));
        assert!(harness.events_after(0, Some(&doomed.id), 100)?.is_empty());
        assert!(!harness.events_after(0, Some(&kept.id), 100)?.is_empty());
        harness.shutdown(std::time::Duration::from_secs(5)).await?;
        // Release every handle on the store, so the reopen below is a fresh open.
        approver.abort();
        let _ = approver.await;
        (doomed, kept, last)
    };
    for table in [
        "sessions",
        "messages",
        "runs",
        "inputs",
        "effects",
        "outputs",
        "events",
        "approvals",
        "approval_rules",
        "context_checkpoints",
        "context_chapters",
    ] {
        assert_eq!(count(&path, table, &doomed.id)?, 0, "{table} is empty");
    }
    assert!(count(&path, "messages", &kept.id)? > 0);
    // Reopening skips the previously reserved range, including deleted events.
    let store = Store::open(&path)?;
    let harness = harness(&store, model)?;
    assert!(harness.last_sequence() > last);
    assert!(!harness.can_resume_from(last));
    let _approver = approve_all(&harness);
    harness
        .prompt(&kept.id, "hello", RunOptions::default())
        .await?;
    let events = harness.events_after(last, Some(&kept.id), 100)?;
    assert!(!events.is_empty());
    assert!(events.iter().all(|e| e.sequence > last));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_session_gives_a_typed_error_and_refuses_deletion() -> Result<()> {
    let store = Store::in_memory()?;
    let model = Arc::new(Model::default());
    let harness = harness(&store, model.clone())?;
    let session = store.create_session("busy")?;
    let run = harness.start(&session.id, "block", RunOptions::default())?;
    let error = harness
        .start(&session.id, "again", RunOptions::default())
        .unwrap_err();
    let busy = error
        .downcast_ref::<SessionBusy>()
        .expect("typed busy error");
    assert_eq!(busy.session_id, session.id);
    assert_eq!(busy.run_id, run.id);
    assert_eq!(
        error.to_string(),
        format!(
            "Session already has an active run {}; steer or follow up instead",
            run.id
        )
    );
    let refused = harness.delete_session(&session.id).await.unwrap_err();
    assert!(refused.downcast_ref::<SessionBusy>().is_some());
    assert!(store.delete_session(&session.id).is_err());
    assert_eq!(model.closed.load(Ordering::Relaxed), 0);
    harness.cancel(&session.id);
    harness.wait_for_idle(&session.id).await?;
    harness.delete_session(&session.id).await?;
    assert!(store.session(&session.id).is_err());
    Ok(())
}
