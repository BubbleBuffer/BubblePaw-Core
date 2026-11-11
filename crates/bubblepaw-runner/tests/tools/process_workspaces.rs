//! Workspace views retain one host-wide process registry and lifetime.
use anyhow::Result;
use bubblepaw_core::{CancellationToken, ToolContext, ToolRegistry};
use bubblepaw_runner::{
    config::ExecConfig,
    tools::{ProcessTools, WorkspaceTools},
    wake::WakeCoordinator,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[cfg(windows)]
const SLEEP: &str = "ping -n 31 127.0.0.1 >nul";
#[cfg(not(windows))]
const SLEEP: &str = "sleep 30";
#[cfg(windows)]
const WRITE: &str = "<nul set /p=child workspace>view.txt& cmd /c exit 0";
#[cfg(not(windows))]
const WRITE: &str = "printf 'child workspace' > view.txt";

fn context(owner: &str) -> ToolContext {
    ToolContext {
        session_id: owner.into(),
        run_id: "fixture".into(),
        invocation_id: uuid::Uuid::new_v4().to_string(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    }
}
fn registry(processes: &Arc<ProcessTools>) -> Result<ToolRegistry> {
    let mut tools = ToolRegistry::new();
    processes.register(&mut tools)?;
    processes.register_wait(&mut tools)?;
    Ok(tools)
}
async fn call(tools: &ToolRegistry, owner: &str, command: &str, background: bool) -> Result<Value> {
    let result = tools
        .get("exec")
        .unwrap()
        .invoke(
            context(owner),
            json!({"command":command,"background":background}),
        )
        .await?;
    assert!(!result.is_error, "{}", result.content);
    Ok(serde_json::from_str(&result.content)?)
}

#[tokio::test]
async fn views_share_live_host_caps_job_visibility_and_late_wake_attachment() -> Result<()> {
    let root_workspace = tempfile::tempdir()?;
    let child_workspace = tempfile::tempdir()?;
    let data = tempfile::tempdir()?;
    let root = Arc::new(ProcessTools::with_config(
        WorkspaceTools::new(root_workspace.path())?,
        ExecConfig::default(),
        data.path(),
    )?);
    let view = Arc::new(root.for_workspace(WorkspaceTools::new(child_workspace.path())?));
    let root_tools = registry(&root)?;
    let child_tools = registry(&view)?;
    let wake = Arc::new(WakeCoordinator::new(false, Duration::ZERO));
    root.set_wake(wake.clone());
    assert!(Arc::ptr_eq(&view.wake_coordinator(), &wake));
    root.apply_config(ExecConfig {
        max_jobs: Some(1),
        ..Default::default()
    });
    call(&root_tools, "parent", SLEEP, true).await?;
    let rejected = call(&child_tools, "child", WRITE, false).await.unwrap_err();
    assert!(
        rejected.to_string().contains("max_jobs limit"),
        "{rejected}"
    );
    assert!(!child_workspace.path().join("view.txt").exists());
    root.cancel_owner("parent").await;
    let result = call(&child_tools, "child", WRITE, false).await?;
    assert_eq!(result["exitCode"], 0);
    assert_eq!(
        std::fs::read_to_string(child_workspace.path().join("view.txt"))?,
        "child workspace"
    );
    assert!(!root_workspace.path().join("view.txt").exists());
    assert_eq!(root.jobs("child")?["jobs"].as_array().unwrap().len(), 1);
    root.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn view_shutdown_and_drop_preserve_siblings_and_cancel_owner_stays_scoped() -> Result<()> {
    let root_workspace = tempfile::tempdir()?;
    let child_workspace = tempfile::tempdir()?;
    let root = Arc::new(ProcessTools::new(WorkspaceTools::new(
        root_workspace.path(),
    )?));
    let view = Arc::new(root.for_workspace(WorkspaceTools::new(child_workspace.path())?));
    let root_tools = registry(&root)?;
    let child_tools = registry(&view)?;
    call(&root_tools, "parent", SLEEP, true).await?;
    call(&child_tools, "child", SLEEP, true).await?;
    view.shutdown().await;
    drop(child_tools);
    drop(view);
    for owner in ["parent", "child"] {
        assert_eq!(root.jobs(owner)?["jobs"][0]["status"], "running");
    }
    root.cancel_owner("child").await;
    assert!(root.jobs("child")?["jobs"].as_array().unwrap().is_empty());
    assert_eq!(root.jobs("parent")?["jobs"][0]["status"], "running");
    root.shutdown().await;
    assert!(root.jobs("parent")?["jobs"].as_array().unwrap().is_empty());
    Ok(())
}
