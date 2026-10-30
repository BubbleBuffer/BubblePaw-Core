//! session.delete through the one-shot command path. The command runs against
//! live ProcessTools and WakeCoordinator instances wired the way `run` wires them.
use super::*;
use bubblepaw_core::{
    ApprovalMode, ApprovalPolicy, CancellationToken, Harness, HarnessConfig, Reminder, Run,
    RunOptions, ToolRegistry,
};
use bubblepaw_runner::{provider::ScriptedProvider, tools::register_native_tools};
use std::sync::Mutex;

fn job_count(processes: &ProcessTools, owner: &str) -> Result<usize> {
    Ok(processes.jobs(owner)?["jobs"].as_array().unwrap().len())
}

#[tokio::test]
async fn one_shot_delete_suppresses_wake_and_cancels_only_its_jobs() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session_with_workspace("delete", root.path())?;
    let other = store.create_session_with_workspace("other", root.path())?;
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, root.path(), false)?;
    let harness = Arc::new(Harness::new(
        store.clone(),
        Arc::new(ScriptedProvider::new("")),
        registry,
        HarnessConfig {
            approval: ApprovalPolicy {
                confirm: ApprovalMode::Allow,
                ..Default::default()
            },
            ..Default::default()
        },
    ));
    #[cfg(windows)]
    let command = "ping -n 31 127.0.0.1 >nul";
    #[cfg(not(windows))]
    let command = "sleep 30";
    for id in [&session.id, &other.id] {
        let reply = harness
            .call_tool(
                id,
                "exec",
                json!({"command":command,"yieldMs":0,"timeoutMs":60000}),
                RunOptions::default(),
                CancellationToken::new(),
            )
            .await?;
        assert!(!reply.is_error, "{}", reply.content);
    }
    assert_eq!(job_count(&processes, &session.id)?, 1);
    assert_eq!(job_count(&processes, &other.id)?, 1);

    // The live coordinator, wired to the same processes the command will cancel.
    let wake = Arc::new(WakeCoordinator::new(false, Duration::ZERO));
    link_processes(&processes, &wake, &store);
    let wake_driver = wake.start_driver();
    // Records each owner the driver tries to deliver to. Errors keep notices queued.
    let delivered = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = delivered.clone();
    let registration = wake.register_reminder_fallback(Arc::new(
        move |owner: &str, _reminders: Vec<Reminder>| -> Result<Run> {
            sink.lock().unwrap().push(owner.to_owned());
            Err(anyhow::anyhow!("recorded by the fixture"))
        },
    ));

    let mut ctx = HostCtx::new(harness, store.clone(), root.path());
    ctx.processes = Some(processes.clone());
    ctx.wake = Some(wake.clone());
    run_with_ctx(
        &Command::Delete {
            session: session.id.clone(),
        },
        ctx,
    )
    .await?;

    assert!(store.session(&session.id).is_err());
    assert_eq!(job_count(&processes, &session.id)?, 0);
    assert_eq!(job_count(&processes, &other.id)?, 1);

    // Both owners now have a notice; the deleted one must never reach the driver.
    wake.publish_required(&session.id, "test", "after delete".into());
    wake.publish_required(&other.id, "test", "after delete".into());
    let mut delivered_other = false;
    for _ in 0..200 {
        if delivered.lock().unwrap().contains(&other.id) {
            delivered_other = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        delivered_other,
        "the surviving session's notice was delivered"
    );
    assert!(
        !delivered.lock().unwrap().contains(&session.id),
        "a deleted session received wake delivery"
    );

    drop(registration);
    wake_driver.shutdown().await;
    processes.shutdown().await;
    Ok(())
}
