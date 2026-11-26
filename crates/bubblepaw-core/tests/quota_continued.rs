//! A run continued from queued input is the owner of the waits it parks, not the run before it.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, RunOptions,
    Store, StreamCallback, ToolRegistry, clear_current_quota_wait, set_quota_wait,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

struct Parking {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}
#[async_trait]
impl Provider for Parking {
    async fn generate(
        &self,
        _request: ModelRequest,
        _: StreamCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        assert!(set_quota_wait(
            "work",
            "weekly",
            "2026-10-21T00:00:00Z",
            "exhausted"
        )?);
        self.entered.notify_one();
        self.release.notified().await;
        assert!(clear_current_quota_wait()?);
        Ok(ModelResponse {
            text: "done".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn a_continued_run_waits_under_its_own_run_id() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("continued")?;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let engine = Harness::new(
        store.clone(),
        Arc::new(Parking {
            entered: entered.clone(),
            release: release.clone(),
        }),
        ToolRegistry::new(),
        HarnessConfig {
            model: "p/m".into(),
            ..Default::default()
        },
    );
    let first = engine.start(&session.id, "first", RunOptions::default())?;
    tokio::time::timeout(Duration::from_secs(3), entered.notified()).await?;
    engine.followup(&session.id, "second")?;
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), entered.notified()).await?;
    let wait = store
        .quota_wait(&session.id)?
        .expect("the continued run parks");
    assert_ne!(wait.run_id, first.id);
    assert_eq!(store.run(&wait.run_id)?.status, "waiting_for_quota");
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), engine.wait_for_idle(&session.id)).await??;
    assert!(store.pending_quota_waits()?.is_empty());
    Ok(())
}
