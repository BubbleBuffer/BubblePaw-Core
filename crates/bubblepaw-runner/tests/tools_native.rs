use bubblepaw_core::{CancellationToken, ToolContext, ToolRegistry};
use bubblepaw_runner::tools::register_native_tools;
use serde_json::{Value, json};
use std::time::Duration;

#[path = "support/checkpoint_write.rs"]
mod checkpoint_write;
#[cfg(windows)]
#[path = "support/windows_command.rs"]
mod windows_command;

fn context() -> ToolContext {
    ToolContext {
        session_id: "test-owner".into(),
        run_id: "test-run".into(),
        invocation_id: "test-call".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    }
}
async fn call(registry: &ToolRegistry, name: &str, input: Value) -> Value {
    if matches!(name, "edit" | "write") {
        return checkpoint_write::invoke(registry, name, input)
            .await
            .unwrap();
    }
    let output = registry
        .get(name)
        .unwrap()
        .invoke(context(), input)
        .await
        .unwrap();
    serde_json::from_str(&output.content).unwrap()
}
#[tokio::test]
async fn root_and_exact_edit_contract() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, directory.path(), false).unwrap();
    call(
        &registry,
        "write",
        json!({"path":"file.txt","content":"before\n"}),
    )
    .await;
    assert!(
        registry
            .get("read")
            .unwrap()
            .invoke(context(), json!({"path":outside.path()}))
            .await
            .is_err()
    );
    assert!(
        checkpoint_write::invoke(
            &registry,
            "write",
            json!({"path":"../escape.txt","content":"oops\n"})
        )
        .await
        .is_err()
    );
    call(
        &registry,
        "edit",
        json!({"path":"file.txt","old_string":"before","new_string":"after"}),
    )
    .await;
    assert!(
        checkpoint_write::invoke(
            &registry,
            "edit",
            json!({"path":"file.txt","old_string":"before","new_string":"stale"})
        )
        .await
        .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("file.txt")).unwrap(),
        "after\n"
    );
    let readonly = registry.read_only().capabilities();
    assert!(readonly.iter().all(|tool| tool.read_only));
    assert!(!readonly.iter().any(|tool| tool.name == "exec"));
    processes.shutdown().await;
}

fn command(script: impl Into<Value>) -> Value {
    script.into()
}

async fn finished(registry: &ToolRegistry, mut output: Value) -> Value {
    for _ in 0..20 {
        if output["status"] != "running" {
            return output;
        }
        output = call(
            registry,
            "poll",
            json!({"handle":output["handle"],"yieldMs":500,"cursor":{"stdout":0,"stderr":0}}),
        )
        .await;
    }
    panic!("process did not finish in bounded test window");
}

#[tokio::test]
async fn processes_are_real_owned_bounded_and_cancelled() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, root.path(), false).unwrap();
    #[cfg(windows)]
    let script = windows_command::powershell_file(
        root.path(),
        "bytes.ps1",
        "[Console]::Out.Write(('x' * 180000)); [Console]::Out.WriteLine('SUCCESS')",
    );
    #[cfg(not(windows))]
    let script = json!("head -c 180000 /dev/zero | tr '\\000' x; echo SUCCESS");
    let result = call(
        &registry,
        "exec",
        json!({"command":command(script),"yieldMs":1000}),
    )
    .await;
    let result = finished(&registry, result).await;
    assert_eq!(result["exitCode"], 0);
    assert!(
        !result["capture"]["stdout"]["gaps"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(result["stdout"].as_str().unwrap().contains("SUCCESS"));
    assert!(result["stdout"].as_str().unwrap().len() <= 16 * 1024);
    #[cfg(windows)]
    let sleep =
        windows_command::powershell_file(root.path(), "sleep.ps1", "Start-Sleep -Seconds 30");
    #[cfg(not(windows))]
    let sleep = json!("sleep 30");
    let running = call(
        &registry,
        "exec",
        json!({"command":command(sleep),"yieldMs":0}),
    )
    .await;
    let mut alien = context();
    alien.session_id = "alien".into();
    assert!(
        registry
            .get("cancel")
            .unwrap()
            .invoke(alien, json!({"handle":running["handle"]}))
            .await
            .is_err()
    );
    let cancelled = call(&registry, "cancel", json!({"handle":running["handle"]})).await;
    assert_eq!(cancelled["status"], "cancelled");
    processes.shutdown().await;
}

#[tokio::test]
async fn process_deadline_and_run_token_kill_children() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, root.path(), false).unwrap();
    #[cfg(windows)]
    let script =
        windows_command::powershell_file(root.path(), "sleep.ps1", "Start-Sleep -Seconds 30");
    #[cfg(not(windows))]
    let script = json!("sleep 30");
    let result = call(
        &registry,
        "exec",
        json!({"command":command(script.clone()),"timeoutMs":50,"yieldMs":1000}),
    )
    .await;
    let result = finished(&registry, result).await;
    assert_eq!(result["status"], "timed_out");
    let run = context();
    let token = run.cancellation.clone();
    let result = registry
        .get("exec")
        .unwrap()
        .invoke(run, json!({"command":command(script),"yieldMs":0}))
        .await
        .unwrap();
    let result: Value = serde_json::from_str(&result.content).unwrap();
    token.cancel();
    let result = tokio::time::timeout(Duration::from_secs(8), finished(&registry, result))
        .await
        .unwrap();
    assert_eq!(result["status"], "cancelled");
    processes.shutdown().await;
}

#[tokio::test]
async fn readonly_registration_has_no_mutation_or_processes() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, root.path(), true).unwrap();
    assert!(registry.definitions().is_empty());
    assert_eq!(registry.capabilities().len(), 4);
    assert!(registry.get("edit").is_none());
    assert!(registry.get("write").is_none());
    assert!(registry.get("exec").is_none());
    processes.shutdown().await;
}

#[cfg(windows)]
#[tokio::test]
async fn windows_cancel_terminates_a_spawned_descendant() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, root.path(), false).unwrap();
    std::fs::write(
        root.path().join("descendant-child.ps1"),
        "Start-Sleep -Seconds 30",
    )
    .unwrap();
    let script = windows_command::powershell_file(
        root.path(),
        "descendant.ps1",
        r#"$childScript = Join-Path $PSScriptRoot 'descendant-child.ps1'; $child = Start-Process -FilePath (Join-Path $PSHOME 'powershell.exe') -WindowStyle Hidden -ArgumentList ('-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "{0}"' -f $childScript) -PassThru; [Console]::Out.WriteLine($child.Id); Start-Sleep -Seconds 30"#,
    );
    let mut running = call(
        &registry,
        "exec",
        json!({"command":command(script),"yieldMs":1000}),
    )
    .await;
    for _ in 0..10 {
        if !running["stdout"].as_str().unwrap().trim().is_empty() {
            break;
        }
        running = call(
            &registry,
            "poll",
            json!({"handle":running["handle"],"yieldMs":500}),
        )
        .await;
    }
    let child_pid: u32 = running["stdout"].as_str().unwrap().trim().parse().unwrap();
    call(&registry, "cancel", json!({"handle":running["handle"]})).await;
    let child_status = tokio::process::Command::new("tasklist.exe")
        .args(["/FI", &format!("PID eq {child_pid}"), "/FO", "CSV", "/NH"])
        .creation_flags(0x08000000)
        .output()
        .await
        .unwrap();
    let list = String::from_utf8_lossy(&child_status.stdout);
    assert!(
        !list.contains(&format!("\"{child_pid}\"")),
        "descendant survived tree cancellation: {list}"
    );
    processes.shutdown().await;
}
