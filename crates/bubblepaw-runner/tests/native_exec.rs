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
        session_id: "owner".into(),
        run_id: "run".into(),
        invocation_id: "call".into(),
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
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, directory.path(), false).unwrap();
    #[cfg(windows)]
    let nonzero = "<nul set /p=unknown option --bogus>&2& exit 7";
    #[cfg(not(windows))]
    let nonzero = "printf 'unknown option --bogus' >&2; exit 7";
    let started = call(&registry, "exec", json!({"command":nonzero,"yieldMs":0})).await;
    let done = call(&registry, "wait", json!({"handle":started["handle"]})).await;
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
    let result: Value = serde_json::from_str(&result.content).unwrap();
    assert!(result["diagnostic"].as_str().unwrap().contains("code 7"));
    let started = call(
        &registry,
        "exec",
        json!({"command":SLEEP,"yieldMs":0,"timeoutMs":20}),
    )
    .await;
    let done = call(&registry, "wait", json!({"handle":started["handle"]})).await;
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
    let started = call(&registry, "exec", json!({"command":invalid,"yieldMs":0})).await;
    call(&registry, "wait", json!({"handle":started["handle"]})).await;
    let out = call(&registry, "poll", json!({"handle":started["handle"]})).await;
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
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, directory.path(), false).unwrap();
    let started = call(
        &registry,
        "exec",
        json!({"command":ECHO,"yieldMs":0,"timeoutMs":600001}),
    )
    .await;
    let done = call(&registry, "wait", json!({"handle":started["handle"]})).await;
    assert_eq!(done["status"], "exited");
    assert_eq!(done["exitCode"], 0);
    let first = call(
        &registry,
        "poll",
        json!({"handle":started["handle"],"cursor":{"stdout":0,"stderr":0}}),
    )
    .await;
    let replay = call(
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
    for name in [
        "exec", "poll", "cancel", "read", "list", "search", "edit", "write",
    ] {
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
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, directory.path(), false).unwrap();
    let started = call(&registry, "exec", json!({"command":SLEEP,"yieldMs":0})).await;
    let timeout = call(
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
        call(&registry, "poll", json!({"handle":started["handle"]})).await["status"],
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
    let cancelled = call(&registry, "cancel", json!({"handle":started["handle"]})).await;
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(
        call(&registry, "wait", json!({"handle":started["handle"]})).await["status"],
        "cancelled"
    );
    processes.shutdown().await;
}

#[tokio::test]
async fn native_gaps_are_explicit_and_bounded() {
    let directory = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, directory.path(), false).unwrap();
    #[cfg(windows)]
    let script = windows_command::powershell_file(
        directory.path(),
        "gaps.ps1",
        "[Console]::Out.Write(('x' * 180000)); [Console]::Out.Write('END')",
    );
    #[cfg(not(windows))]
    let script = json!("head -c 180000 /dev/zero | tr '\\000' x; printf END");
    let started = call(&registry, "exec", json!({"command":script,"yieldMs":0})).await;
    call(&registry, "wait", json!({"handle":started["handle"]})).await;
    let out = call(&registry, "poll", json!({"handle":started["handle"]})).await;
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
            &registry,
            "poll",
            json!({"handle":started["handle"],"cursor":{"stdout":0,"stderr":0}})
        )
        .await
    );
    let consumed = call(
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
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, directory.path(), false).unwrap();
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
        ("list", json!({"limit":"bad"})),
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
        call(&registry, "wait", json!({"ms":0})).await["status"],
        "completed"
    );
    processes.shutdown().await;
}

#[tokio::test]
async fn native_read_edit_and_sorted_list_contract() {
    let directory = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, directory.path(), false).unwrap();
    for name in ["z.txt", "b.txt", "a.txt"] {
        call(
            &registry,
            "write",
            json!({"path":name,"content":"one\ntwo\nthree\n"}),
        )
        .await;
    }
    assert_eq!(
        call(&registry, "list", json!({"limit":1})).await["entries"][0]["name"],
        "a.txt"
    );
    let read = call(
        &registry,
        "read",
        json!({"path":"a.txt","startLine":2,"lineCount":1}),
    )
    .await;
    assert_eq!(read["content"], "two\n");
    assert_eq!(read["totalLines"], 3);
    assert_eq!(read["returnedLines"], 1);
    call(
        &registry,
        "edit",
        json!({"path":"a.txt","old_string":"two","new_string":"changed"}),
    )
    .await;
    let error = checkpoint_write::invoke(
        &registry,
        "edit",
        json!({"path":"a.txt","old_string":"two","new_string":"stale"}),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("no exact match"), "{error}");
    processes.shutdown().await;
}

#[tokio::test]
async fn native_explicit_deadline_is_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, directory.path(), false).unwrap();
    let started = call(
        &registry,
        "exec",
        json!({"command":SLEEP,"timeoutMs":10,"yieldMs":0}),
    )
    .await;
    assert_eq!(
        call(&registry, "wait", json!({"handle":started["handle"]})).await["status"],
        "timed_out"
    );
    processes.shutdown().await;
}

#[tokio::test]
async fn session_cancel_hook_stops_detached_processes_once() {
    let directory = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, directory.path(), false).unwrap();
    let started = call(&registry, "exec", json!({"command":SLEEP,"yieldMs":0})).await;
    assert!(registry.get("process_run").is_none());
    assert_eq!(registry.get("exec").unwrap().cancel_session("alien"), 0);
    assert_eq!(registry.get("exec").unwrap().cancel_session("owner"), 1);
    assert_eq!(registry.get("exec").unwrap().cancel_session("owner"), 0);
    let done = call(&registry, "wait", json!({"handle":started["handle"]})).await;
    assert_eq!(done["status"], "cancelled");
    processes.shutdown().await;
}
