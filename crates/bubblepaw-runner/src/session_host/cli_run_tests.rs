//! Cancellation through the one-shot CLI adapter. The token given to
//! `run_session` must reach the table's `run.cancel` and cascade to the run's
//! running child and pending question.
use super::*;
use bubblepaw_core::{Harness, Store, ToolContext};
use bubblepaw_runner::{agents::AgentTools, question::QuestionTools};
use std::sync::Arc;
use tokio::sync::mpsc;

/// A provider that never answers until its run is cancelled.
struct Pending;

#[async_trait::async_trait]
impl bubblepaw_core::Provider for Pending {
    async fn generate(
        &self,
        _request: bubblepaw_core::ModelRequest,
        _text: bubblepaw_core::TextCallback,
        cancel: CancellationToken,
    ) -> anyhow::Result<bubblepaw_core::ModelResponse> {
        cancel.cancelled().await;
        bail!("cancelled fixture")
    }
}

/// The session's running run, once the adapter has admitted it.
async fn running_run(store: &Store, session: &str) -> Result<String> {
    for _ in 0..500 {
        if let Some(run) = store
            .runs(session)?
            .into_iter()
            .find(|run| run.status == "running")
        {
            return Ok(run.id);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    bail!("run never started")
}

#[tokio::test]
async fn cli_cancel_reaches_running_child_and_pending_question() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session_with_workspace("cli", root.path())?;
    let provider = Arc::new(Pending);
    let harness = Arc::new(Harness::new(
        store.clone(),
        provider.clone(),
        Default::default(),
        Default::default(),
    ));
    let child_harness = Harness::new(
        store.clone(),
        provider,
        Default::default(),
        Default::default(),
    );
    let agents = Arc::new(AgentTools::new(store.clone(), child_harness, None));
    let wake = agents.wake_coordinator();
    let questions = Arc::new(QuestionTools::new(store.clone(), wake.clone(), true));
    let mut ctx = HostCtx::new(harness, store.clone(), root.path());
    ctx.agents = Some(agents.clone());
    ctx.questions = Some(questions);
    ctx.wake = Some(wake);

    let cancellation = CancellationToken::new();
    // Keep the receiver alive so presentation never fails and cancels on its own.
    let (sender, _records) = mpsc::channel(1024);
    let output = Output::remote(sender, cancellation.clone());
    let adapter = run_session(
        &ctx,
        &session.id,
        OneShot::Prompt("wait"),
        false,
        output,
        cancellation.clone(),
    );
    // The adapter and this scenario share one task, so no spawn is needed.
    let scenario = async {
        let run_id = running_run(&store, &session.id).await?;
        let child = agents.start(
            &ToolContext {
                session_id: session.id.clone(),
                run_id: run_id.clone(),
                invocation_id: "cli-child".into(),
                cancellation: CancellationToken::new(),
                approval: Default::default(),
            },
            json!({"task":"wait"}),
        )?;
        store.update_host_state(&session.id, &run_id, "question", |_| {
            Ok(json!({"id":"pending-question","status":"pending","text":"answer?","options":[],"run_id":run_id,"parent_session_id":null}))
        })?;
        cancellation.cancel();
        Ok::<_, anyhow::Error>(child)
    };
    let (adapter_result, scenario_result) = tokio::join!(adapter, scenario);
    let child = scenario_result?;

    let error = adapter_result.expect_err("a cancelled run must not succeed");
    assert!(
        error.to_string().starts_with("run cancelled"),
        "unexpected adapter error: {error}"
    );
    assert_eq!(
        store.host_state(&session.id, "question")?.unwrap()["status"],
        "cancelled"
    );
    let child_result = agents
        .wait(&session.id, child["handle"].as_str().unwrap(), Some(2000))
        .await?;
    assert_eq!(child_result["status"], "cancelled");

    // No worker outlives the adapter: no run of this session or its children is still running.
    let mut settled = vec![session.id.clone()];
    settled.extend(
        store
            .list_sessions()?
            .into_iter()
            .filter(|s| s.parent_session_id.as_deref() == Some(session.id.as_str()))
            .map(|s| s.id),
    );
    assert!(settled.len() >= 2, "the child session is recorded");
    for id in &settled {
        for run in store.runs(id)? {
            assert_ne!(
                run.status, "running",
                "run {} of {id} still running",
                run.id
            );
        }
    }
    agents.shutdown().await?;
    Ok(())
}
