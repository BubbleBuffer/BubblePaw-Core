use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, RunOptions,
    Store, TextCallback, ToolRegistry,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Panics on the first request, then answers like an ordinary provider.
struct PanicsOnce {
    calls: AtomicUsize,
}

#[async_trait]
impl Provider for PanicsOnce {
    async fn generate(
        &self,
        _: ModelRequest,
        on_text: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("scripted provider exploded");
        }
        on_text("recovered");
        Ok(ModelResponse {
            text: "recovered".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn panic_in_run_returns_error() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("panic")?;
    let harness = Harness::new(
        store.clone(),
        Arc::new(PanicsOnce {
            calls: AtomicUsize::new(0),
        }),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    // A panicking turn must settle the run instead of leaving the waiter blocked.
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        harness.prompt(&session.id, "explode", RunOptions::default()),
    )
    .await
    .expect("prompt hung after a panicking run")
    .expect_err("a panicking run must fail the prompt");
    let message = format!("{error:#}");
    assert!(message.contains("scripted provider exploded"), "{message}");
    let runs = store.runs(&session.id)?;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "failed");
    let stored = runs[0].error.as_deref().unwrap_or_default();
    assert!(stored.contains("scripted provider exploded"), "{stored}");
    // The session stays usable: the same harness runs its next prompt normally.
    let run = tokio::time::timeout(
        Duration::from_secs(5),
        harness.prompt(&session.id, "again", RunOptions::default()),
    )
    .await
    .expect("a second prompt hung after a panicked run")?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(run.text.as_deref(), Some("recovered"));
    Ok(())
}
