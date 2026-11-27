use bubblepaw_core::{CancellationToken, ToolContext, ToolRegistry};
use serde_json::{Value, json};
use std::time::Duration;

#[allow(unused_imports)]
use crate::checkpoint_write;
#[cfg(windows)]
#[allow(unused_imports)]
use crate::windows_command;

fn context() -> ToolContext {
    ToolContext {
        session_id: "owner".into(),
        run_id: "run".into(),
        invocation_id: "call".into(),
        cancellation: CancellationToken::new(),
        extensions: Default::default(),
    }
}
async fn call(
    store: &bubblepaw_core::Store,
    registry: &ToolRegistry,
    name: &str,
    input: Value,
) -> Value {
    if matches!(name, "edit" | "write") {
        return checkpoint_write::invoke(store, registry, name, input)
            .await
            .unwrap();
    }
    let output = registry
        .get(name)
        .unwrap()
        .invoke(context(), input)
        .await
        .unwrap();
    serde_json::from_str(&output.body()).unwrap()
}
#[cfg(windows)]
const ECHO: &str = "<nul set /p=hello& <nul set /p=problem>&2& cmd /c exit 0";
#[cfg(not(windows))]
const ECHO: &str = "printf hello; printf problem >&2";
#[cfg(windows)]
const SLEEP: &str = "ping -n 31 127.0.0.1 >nul";
#[cfg(not(windows))]
const SLEEP: &str = "sleep 30";

#[tokio::test]
async fn command_diagnostics_distinguish_nonzero_timeout_and_decoding_loss() {
    let directory = tempfile::tempdir().unwrap();
    let store = bubblepaw_core::Store::in_memory().unwrap();
    let mut registry = ToolRegistry::new();
    let processes =
        checkpoint_write::native_pack(&mut registry, directory.path(), false, &store).unwrap();
    #[cfg(windows)]
    let nonzero = "<nul set /p=unknown option --bogus>&2& exit 7";
    #[cfg(not(windows))]
    let nonzero = "printf 'unknown option --bogus' >&2; exit 7";
    let started = call(
        &store,
        &registry,
        "exec",
        json!({"command":nonzero,"yieldMs":0}),
    )
    .await;
    let done = call(
        &store,
        &registry,
        "wait",
        json!({"handle":started["handle"]}),
    )
    .await;
    assert_eq!(done["exitCode"], 7);
    assert!(
        done["diagnostic"]
            .as_str()
            .unwrap()
            .contains("unknown option --bogus")
    );
    let result = registry
        .get("poll")
        .unwrap()
        .invoke(context(), json!({"handle":started["handle"]}))
        .await
        .unwrap();
    assert!(
        !result.is_error,
        "nonzero process exit remains an ordinary result"
    );
    let result: Value = serde_json::from_str(&result.body()).unwrap();
    assert!(result["diagnostic"].as_str().unwrap().contains("code 7"));
    let started = call(
        &store,
        &registry,
        "exec",
        json!({"command":SLEEP,"yieldMs":0,"timeoutMs":20}),
    )
    .await;
    let done = call(
        &store,
        &registry,
        "wait",
        json!({"handle":started["handle"]}),
    )
    .await;
    assert_eq!(done["status"], "timed_out");
    assert!(done["diagnostic"].as_str().unwrap().contains("terminated"));
    #[cfg(windows)]
    let invalid = windows_command::powershell_file(
        directory.path(),
        "invalid-bytes.ps1",
        "[Console]::OpenStandardOutput().Write([byte[]]@(255,254),0,2)",
    );
    #[cfg(not(windows))]
    let invalid = json!("printf '\\377\\376'");
    let started = call(
        &store,
        &registry,
        "exec",
        json!({"command":invalid,"yieldMs":0}),
    )
    .await;
    call(
        &store,
        &registry,
        "wait",
        json!({"handle":started["handle"]}),
    )
    .await;
    let out = call(
        &store,
        &registry,
        "poll",
        json!({"handle":started["handle"]}),
    )
    .await;
    assert_eq!(out["exitCode"], 0);
    assert_eq!(out["capture"]["stdout"]["lossyUtf8"], true);
    assert_eq!(out["capture"]["stdout"]["gaps"], json!([]));
    let diagnostic = out["diagnostic"].as_str().unwrap();
    assert!(diagnostic.contains("Command succeeded"));
    assert!(diagnostic.contains("decoding is lossy"));
    assert!(!diagnostic.contains("Capture is incomplete"));
    processes.shutdown().await;
}

#[tokio::test]
async fn native_shell_cursors_replay_without_consumption() {
    let directory = tempfile::tempdir().unwrap();
    let store = bubblepaw_core::Store::in_memory().unwrap();
    let mut registry = ToolRegistry::new();
    let processes =
        checkpoint_write::native_pack(&mut registry, directory.path(), false, &store).unwrap();
    let started = call(
        &store,
        &registry,
        "exec",
        json!({"command":ECHO,"yieldMs":0,"timeoutMs":600001}),
    )
    .await;
    let done = call(
        &store,
        &registry,
        "wait",
        json!({"handle":started["handle"]}),
    )
    .await;
    assert_eq!(done["status"], "exited");
    assert_eq!(done["exitCode"], 0);
    let first = call(
        &store,
        &registry,
        "poll",
        json!({"handle":started["handle"],"cursor":{"stdout":0,"stderr":0}}),
    )
    .await;
    let replay = call(
        &store,
        &registry,
        "poll",
        json!({"handle":started["handle"],"cursor":{"stdout":0,"stderr":0}}),
    )
    .await;
    assert_eq!(first, replay);
    assert_eq!(first["stdout"], "hello");
    assert_eq!(first["stderr"], "problem");
    assert!(first.get("diagnostic").is_none());
    let next = call(
        &store,
        &registry,
        "poll",
        json!({"handle":started["handle"],"cursor":first["cursor"]}),
    )
    .await;
    assert_eq!(next["stdout"], "");
    assert_eq!(next["stderr"], "");
    let names = registry
        .capabilities()
        .into_iter()
        .map(|d| d.name)
        .collect::<Vec<_>>();
    for name in ["exec", "poll", "cancel", "read", "search", "edit", "write"] {
        assert!(names.iter().any(|n| n == name));
    }
    assert!(
        !names
            .iter()
            .any(|n| n.starts_with("process_") || n.starts_with("workspace_"))
    );
    assert!(
        registry
            .capabilities()
            .iter()
            .any(|d| d.name == "wait" && d.read_only)
    );
    processes.shutdown().await;
}

#[tokio::test]
async fn native_wait_timeout_keeps_process_alive_and_session_isolation() {
    let directory = tempfile::tempdir().unwrap();
    let store = bubblepaw_core::Store::in_memory().unwrap();
    let mut registry = ToolRegistry::new();
    let processes =
        checkpoint_write::native_pack(&mut registry, directory.path(), false, &store).unwrap();
    let started = call(
        &store,
        &registry,
        "exec",
        json!({"command":SLEEP,"yieldMs":0}),
    )
    .await;
    let timeout = call(
        &store,
        &registry,
        "wait",
        json!({"handle":started["handle"],"timeoutMs":5}),
    )
    .await;
    assert_eq!(timeout["status"], "timeout");
    assert!(
        timeout["diagnostic"]
            .as_str()
            .unwrap()
            .contains("command is still running")
    );
    assert_eq!(
        call(
            &store,
            &registry,
            "poll",
            json!({"handle":started["handle"]})
        )
        .await["status"],
        "running"
    );
    let mut alien = context();
    alien.session_id = "alien".into();
    assert!(
        registry
            .get("wait")
            .unwrap()
            .invoke(alien, json!({"handle":started["handle"]}))
            .await
            .is_err()
    );
    assert!(
        registry
            .get("wait")
            .unwrap()
            .invoke(context(), json!({"handle":"cell:other"}))
            .await
            .unwrap_err()
            .to_string()
            .contains("unsupported handle")
    );
    let cancelled = call(
        &store,
        &registry,
        "cancel",
        json!({"handle":started["handle"]}),
    )
    .await;
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(
        call(
            &store,
            &registry,
            "wait",
            json!({"handle":started["handle"]})
        )
        .await["status"],
        "cancelled"
    );
    processes.shutdown().await;
}

#[tokio::test]
async fn native_gaps_are_explicit_and_bounded() {
    let directory = tempfile::tempdir().unwrap();
    let store = bubblepaw_core::Store::in_memory().unwrap();
    let mut registry = ToolRegistry::new();
    let processes =
        checkpoint_write::native_pack(&mut registry, directory.path(), false, &store).unwrap();
    #[cfg(windows)]
    let script = windows_command::powershell_file(
        directory.path(),
        "gaps.ps1",
        "[Console]::Out.Write(('x' * 180000)); [Console]::Out.Write('END')",
    );
    #[cfg(not(windows))]
    let script = json!("head -c 180000 /dev/zero | tr '\\000' x; printf END");
    let started = call(
        &store,
        &registry,
        "exec",
        json!({"command":script,"yieldMs":0}),
    )
    .await;
    call(
        &store,
        &registry,
        "wait",
        json!({"handle":started["handle"]}),
    )
    .await;
    let out = call(
        &store,
        &registry,
        "poll",
        json!({"handle":started["handle"]}),
    )
    .await;
    assert_eq!(out["cursor"]["stdout"], 180003);
    assert!(
        out["diagnostic"]
            .as_str()
            .unwrap()
            .contains("Command succeeded (exit code 0)")
    );
    assert!(
        out["diagnostic"]
            .as_str()
            .unwrap()
            .contains("163875 omitted bytes")
    );
    assert!(out["stdout"].as_str().unwrap().len() <= 16 * 1024);
    assert!(out["stdout"].as_str().unwrap().ends_with("END"));
    assert!(out["stdout"].as_str().unwrap().contains("omitted bytes"));
    assert_eq!(
        out["capture"]["stdout"]["gaps"],
        json!([{"from":8064,"to":171939}])
    );
    let middle = call(
        &store,
        &registry,
        "poll",
        json!({"handle":started["handle"],"cursor":{"stdout":40000,"stderr":0}}),
    )
    .await;
    assert_eq!(middle["capture"]["stdout"]["gaps"][0]["from"], 40000);
    assert!(
        middle["stdout"]
            .as_str()
            .unwrap()
            .starts_with("\n[... omitted bytes 40000..")
    );
    assert_eq!(
        out,
        call(
            &store,
            &registry,
            "poll",
            json!({"handle":started["handle"],"cursor":{"stdout":0,"stderr":0}})
        )
        .await
    );
    let consumed = call(
        &store,
        &registry,
        "poll",
        json!({"handle":started["handle"],"cursor":out["cursor"]}),
    )
    .await;
    assert_eq!(consumed["stdout"], "");
    assert_eq!(consumed["capture"]["stdout"]["gaps"], json!([]));
    processes.shutdown().await;
}

#[tokio::test]
async fn long_timer_cancels_and_bad_types_are_errors() {
    let directory = tempfile::tempdir().unwrap();
    let store = bubblepaw_core::Store::in_memory().unwrap();
    let mut registry = ToolRegistry::new();
    let processes =
        checkpoint_write::native_pack(&mut registry, directory.path(), false, &store).unwrap();
    let run = context();
    let token = run.cancellation.clone();
    let waiter = registry.get("wait").unwrap();
    let task = tokio::spawn(async move { waiter.invoke(run, json!({"ms":86400000})).await });
    tokio::task::yield_now().await;
    token.cancel();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    for (name, input) in [
        ("exec", json!({"command":ECHO,"yieldMs":"bad"})),
        ("exec", json!({"command":ECHO,"timeoutMs":0})),
        ("wait", json!({"ms":-1})),
        ("wait", json!({"ms":0,"handle":"process:x"})),
        ("search", json!({"maxResults":"bad"})),
        ("read", json!({"path":"x","startLine":null})),
        ("search", json!({"query":"x","regex":"bad"})),
    ] {
        assert!(
            registry
                .get(name)
                .unwrap()
                .invoke(context(), input)
                .await
                .is_err(),
            "accepted invalid {name}"
        );
    }
    assert_eq!(
        call(&store, &registry, "wait", json!({"ms":0})).await["status"],
        "completed"
    );
    processes.shutdown().await;
}

#[path = "native_exec/contract.rs"]
mod contract;
