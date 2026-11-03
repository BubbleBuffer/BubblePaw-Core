//! `session.list` rows carry the workspace and the sidebar's attention flags.
use super::*;
use anyhow::Result;
use bubblepaw_core::{CancellationToken, EventKind, ToolCall, ToolRegistry};

struct Hold;
#[async_trait::async_trait]
impl bubblepaw_core::Provider for Hold {
    async fn generate(
        &self,
        _request: bubblepaw_core::ModelRequest,
        _text: bubblepaw_core::TextCallback,
        cancel: CancellationToken,
    ) -> Result<bubblepaw_core::ModelResponse> {
        cancel.cancelled().await;
        anyhow::bail!("cancelled fixture")
    }
}

struct Gate;
#[async_trait::async_trait]
impl bubblepaw_core::Tool for Gate {
    fn definition(&self) -> bubblepaw_core::ToolDefinition {
        bubblepaw_core::ToolDefinition {
            name: "row_gate".into(),
            description: "Fixture requiring an explicit decision".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            read_only: false,
            approval: bubblepaw_core::Approval::Confirm,
        }
    }
    async fn invoke(
        &self,
        _context: bubblepaw_core::ToolContext,
        _input: Value,
    ) -> Result<bubblepaw_core::ToolResult> {
        Ok(bubblepaw_core::ToolResult::text("ok"))
    }
}

/// The wire rows of `session.list`, unpaged and paged, which must agree.
async fn rows(ctx: &HostCtx, id: &str) -> Result<Value> {
    let all = dispatch(ctx, Profile::Read, "session.list", json!({})).await?;
    let paged = dispatch(ctx, Profile::Read, "session.list", json!({"limit": 100})).await?;
    let find = |rows: &Value| {
        rows.as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"] == id).cloned())
    };
    let all = find(&all).expect("row in the unpaged result");
    let paged = find(&paged["sessions"]).expect("row in the paged result");
    assert_eq!(all, paged);
    Ok(all)
}

#[tokio::test]
async fn idle_row_has_its_workspace_and_no_attention_flags() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session_with_workspace("idle", root.path())?;
    let ctx = HostCtx::for_store(store, root.path());
    let row = rows(&ctx, &session.id).await?;
    let workspace = row["workspace"].as_str().expect("workspace is a path");
    assert_eq!(
        std::fs::canonicalize(workspace)?,
        std::fs::canonicalize(root.path())?
    );
    assert_eq!(row["needs_you"], false);
    assert_eq!(row["running"], false);
    assert!(row["usage"].is_object(), "existing fields stay: {row}");
    Ok(())
}

#[tokio::test]
async fn pending_question_sets_needs_you() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session_with_workspace("question", root.path())?;
    let ctx = HostCtx::for_store(store.clone(), root.path());
    store.update_host_state(&session.id, "row-fixture", "question", |_| {
        Ok(json!({"id":"question","status":"pending","text":"answer?"}))
    })?;
    let row = rows(&ctx, &session.id).await?;
    assert_eq!(row["needs_you"], true);
    assert_eq!(row["running"], false);
    store.update_host_state(&session.id, "row-fixture", "question", |_| {
        Ok(json!({"id":"question","status":"answered","text":"answer?"}))
    })?;
    assert_eq!(rows(&ctx, &session.id).await?["needs_you"], false);
    Ok(())
}

#[tokio::test]
async fn running_turn_sets_running() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session_with_workspace("running", root.path())?;
    let harness = Arc::new(Harness::new(
        store.clone(),
        Arc::new(Hold),
        Default::default(),
        Default::default(),
    ));
    let ctx = HostCtx::new(harness, store, root.path());
    dispatch(
        &ctx,
        Profile::Edit,
        "run.start",
        json!({"session_id":session.id,"text":"hold"}),
    )
    .await?;
    let row = rows(&ctx, &session.id).await?;
    assert_eq!(row["running"], true);
    assert_eq!(row["needs_you"], false);
    dispatch(
        &ctx,
        Profile::Edit,
        "run.cancel",
        json!({"session_id":session.id}),
    )
    .await?;
    dispatch(
        &ctx,
        Profile::Read,
        "run.wait",
        json!({"session_id":session.id}),
    )
    .await?;
    assert_eq!(rows(&ctx, &session.id).await?["running"], false);
    Ok(())
}

#[tokio::test]
async fn pending_approval_sets_needs_you() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session_with_workspace("approval", root.path())?;
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(Gate))?;
    let provider = crate::provider::ScriptedProvider::with_steps(
        vec![crate::provider::ScriptedStep::Calls {
            calls: vec![ToolCall {
                id: "gate".into(),
                name: "row_gate".into(),
                arguments: json!({}),
            }],
        }],
        "done",
    );
    let harness = Arc::new(Harness::new(
        store.clone(),
        Arc::new(provider),
        registry,
        Default::default(),
    ));
    let ctx = HostCtx::new(harness, store, root.path());
    let mut events = events::subscribe(&ctx, None, Some(&session.id))?.receiver;
    dispatch(
        &ctx,
        Profile::Edit,
        "run.start",
        json!({"session_id":session.id,"text":"gate"}),
    )
    .await?;
    let approval = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let EventKind::ApprovalRequested { approval } = events.recv().await?.kind {
                return Ok::<_, anyhow::Error>(approval);
            }
        }
    })
    .await??;
    let row = rows(&ctx, &session.id).await?;
    assert_eq!(row["needs_you"], true);
    assert_eq!(row["running"], true);
    dispatch(
        &ctx,
        Profile::Edit,
        "approval.decide",
        json!({"approval_id":approval.id,"decision":"deny","reason":"fixture"}),
    )
    .await?;
    dispatch(
        &ctx,
        Profile::Read,
        "run.wait",
        json!({"session_id":session.id}),
    )
    .await?;
    let row = rows(&ctx, &session.id).await?;
    assert_eq!(row["needs_you"], false);
    assert_eq!(row["running"], false);
    Ok(())
}
