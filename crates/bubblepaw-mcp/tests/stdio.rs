#[path = "stdio/deferral.rs"]
mod deferral;

use bubblepaw_core::{Approval, CancellationToken, Tool, ToolContext, ToolResult};
use bubblepaw_mcp::{McpClientEnv, McpHost, McpServerSettings, McpSettings, McpToolSettings};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

fn config(args: Vec<String>) -> McpServerSettings {
    McpServerSettings {
        command: env!("CARGO_BIN_EXE_bubblepaw-fake-mcp").into(),
        args,
        startup_timeout_secs: 3,
        call_timeout_secs: 1,
        ..Default::default()
    }
}
async fn start(config: McpServerSettings, workspace: &Path) -> McpHost {
    McpHost::start(
        &McpSettings {
            servers: BTreeMap::from([("fixture".into(), config)]),
            ..Default::default()
        },
        workspace,
    )
    .await
}
fn tool(host: &McpHost, name: &str) -> Arc<dyn Tool> {
    host.tools()
        .into_iter()
        .find(|tool| tool.definition().name == format!("mcp__fixture__{name}"))
        .unwrap_or_else(|| panic!("missing {name}: {:?}", host.warnings()))
}
fn context() -> ToolContext {
    ToolContext {
        session_id: "test-session".into(),
        run_id: "test-run".into(),
        invocation_id: "test-invocation".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    }
}
async fn invoke(host: &McpHost, name: &str, input: Value) -> ToolResult {
    tool(host, name).invoke(context(), input).await.unwrap()
}
fn requests(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}
async fn eventually(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(4);
    while !condition() {
        assert!(Instant::now() < deadline, "condition did not become true");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn filters_tools_skips_invalid_schemas_and_name_collisions() {
    let root = tempfile::tempdir().unwrap();
    let host = start(
        config(vec!["--invalid-schema".into(), "--collision".into()]),
        root.path(),
    )
    .await;
    assert_eq!(host.warnings().len(), 2);
    assert!(
        host.warnings()
            .iter()
            .any(|warning| warning.contains("invalid input schema"))
    );
    assert!(
        host.warnings()
            .iter()
            .any(|warning| warning.contains("duplicate"))
    );
    host.shutdown(Duration::ZERO).await;
    let mut settings = config(Vec::new());
    settings.include = vec!["echo".into(), "fail".into(), "image".into()];
    settings.exclude = vec!["image".into()];
    settings.approval = Approval::None;
    settings.tools.insert(
        "fail".into(),
        McpToolSettings {
            approval: Some(Approval::Confirm),
        },
    );
    let host = start(settings, root.path()).await;
    assert_eq!(host.tools().len(), 2);
    assert_eq!(tool(&host, "echo").definition().approval, Approval::None);
    assert_eq!(tool(&host, "fail").definition().approval, Approval::Confirm);
    host.shutdown(Duration::ZERO).await;
}

#[tokio::test]
async fn startup_failures_are_isolated_bounded_and_protocol_versions_checked() {
    let root = tempfile::tempdir().unwrap();
    let mut servers = BTreeMap::new();
    servers.insert("good".into(), config(Vec::new()));
    servers.insert(
        "old".into(),
        config(vec!["--version".into(), "2024-11-05".into()]),
    );
    servers.insert(
        "unsupported".into(),
        config(vec!["--version".into(), "future".into()]),
    );
    servers.insert(
        "malformed".into(),
        config(vec!["--malformed-initialize".into()]),
    );
    servers.insert("pages".into(), config(vec!["--repeat-cursor".into()]));
    servers.insert(
        "missing".into(),
        McpServerSettings {
            command: "bubblepaw-intentionally-missing-command".into(),
            ..Default::default()
        },
    );
    servers.insert(
        "disabled".into(),
        McpServerSettings {
            enabled: false,
            ..Default::default()
        },
    );
    for name in ["slow1", "slow2", "slow3"] {
        let mut settings = config(vec!["--initialize-delay-ms".into(), "5000".into()]);
        settings.startup_timeout_secs = 1;
        servers.insert(name.into(), settings);
    }
    let before = Instant::now();
    let host = McpHost::start(
        &McpSettings {
            servers,
            ..Default::default()
        },
        root.path(),
    )
    .await;
    assert!(
        before.elapsed() < Duration::from_millis(2500),
        "startup was not parallel: {:?}",
        before.elapsed()
    );
    assert_eq!(host.tools().len(), 26);
    assert_eq!(host.warnings().len(), 7, "{:?}", host.warnings());
    assert!(
        host.warnings()
            .iter()
            .any(|warning| warning.contains("incompatible"))
    );
    assert!(
        host.warnings()
            .iter()
            .any(|warning| warning.contains("cursor"))
    );
    host.shutdown(Duration::ZERO).await;
}

#[tokio::test]
async fn timeout_cancels_without_replay_and_parallel_calls_are_correlated() {
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("requests.jsonl");
    let host = start(
        config(vec!["--log".into(), log.display().to_string()]),
        root.path(),
    )
    .await;
    let slow = tool(&host, "slow");
    let pending =
        tokio::spawn(async move { slow.invoke(context(), json!({"ms":5000})).await.unwrap() });
    eventually(|| {
        requests(&log)
            .iter()
            .any(|line| line["request"]["method"] == "tools/call")
    })
    .await;
    assert_eq!(
        invoke(&host, "echo", json!({"text":"parallel"}))
            .await
            .content,
        "parallel"
    );
    let timed_out = pending.await.unwrap();
    assert!(timed_out.is_error);
    assert!(!timed_out.pending);
    assert!(timed_out.content.contains("outcome unknown"));
    eventually(|| {
        requests(&log)
            .iter()
            .any(|line| line["request"]["method"] == "notifications/cancelled")
    })
    .await;
    assert_eq!(
        requests(&log)
            .iter()
            .filter(|line| line["request"]["params"]["name"] == "slow")
            .count(),
        1
    );
    assert_eq!(
        invoke(&host, "echo", json!({"text":"still alive"}))
            .await
            .content,
        "still alive"
    );
    host.shutdown(Duration::ZERO).await;
}

#[tokio::test]
async fn dropping_a_call_sends_cancelled_notification() {
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("requests.jsonl");
    let host = start(
        config(vec!["--log".into(), log.display().to_string()]),
        root.path(),
    )
    .await;
    let slow = tool(&host, "slow");
    let pending = tokio::spawn(async move { slow.invoke(context(), json!({"ms":5000})).await });
    eventually(|| {
        requests(&log)
            .iter()
            .any(|line| line["request"]["method"] == "tools/call")
    })
    .await;
    let id = requests(&log)
        .into_iter()
        .find(|line| line["request"]["method"] == "tools/call")
        .unwrap()["request"]["id"]
        .clone();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    eventually(|| {
        requests(&log).iter().any(|line| {
            line["request"]["method"] == "notifications/cancelled"
                && line["request"]["params"]["requestId"] == id
        })
    })
    .await;
    let cancelled: Value =
        serde_json::from_str(&invoke(&host, "cancelled", json!({})).await.content).unwrap();
    assert!(cancelled.as_array().unwrap().contains(&id));
    host.shutdown(Duration::ZERO).await;
}

#[tokio::test]
async fn crash_fails_pending_call_and_next_call_reinitializes_and_relists() {
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("requests.jsonl");
    let host = start(
        config(vec!["--log".into(), log.display().to_string()]),
        root.path(),
    )
    .await;
    let first_pid = invoke(&host, "pid", json!({})).await.content;
    let crash = invoke(&host, "crash", json!({})).await;
    assert!(crash.is_error);
    assert!(crash.content.contains("server exited; outcome unknown"));
    let second_pid = invoke(&host, "pid", json!({})).await.content;
    assert_ne!(first_pid, second_pid);
    assert_eq!(
        requests(&log)
            .iter()
            .filter(|line| line["request"]["method"] == "initialize")
            .count(),
        2
    );
    assert_eq!(
        requests(&log)
            .iter()
            .filter(|line| line["request"]["method"] == "tools/list")
            .count(),
        2
    );
    assert_eq!(
        requests(&log)
            .iter()
            .filter(|line| line["request"]["params"]["name"] == "crash")
            .count(),
        1
    );
    host.shutdown(Duration::ZERO).await;
}

#[tokio::test]
async fn cancelling_restart_setup_cannot_reuse_a_half_initialized_connection() {
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("requests.jsonl");
    let host = start(
        config(vec![
            "--log".into(),
            log.display().to_string(),
            "--initialize-delay-ms".into(),
            "300".into(),
        ]),
        root.path(),
    )
    .await;
    assert!(invoke(&host, "crash", json!({})).await.is_error);
    let echo = tool(&host, "echo");
    let pending =
        tokio::spawn(async move { echo.invoke(context(), json!({"text":"abandoned"})).await });
    eventually(|| {
        requests(&log)
            .iter()
            .filter(|line| line["request"]["method"] == "initialize")
            .count()
            == 2
    })
    .await;
    pending.abort();
    let _ = pending.await;
    assert_eq!(
        invoke(&host, "echo", json!({"text":"fresh"})).await.content,
        "fresh"
    );
    let lines = requests(&log);
    assert_eq!(
        lines
            .iter()
            .filter(|line| line["request"]["method"] == "initialize")
            .count(),
        3
    );
    assert!(
        !lines
            .iter()
            .any(|line| line["request"]["params"]["arguments"]["text"] == "abandoned")
    );
    let call = lines
        .iter()
        .find(|line| line["request"]["params"]["arguments"]["text"] == "fresh")
        .unwrap();
    assert!(lines.iter().any(|line| line["pid"] == call["pid"]
        && line["request"]["method"] == "notifications/initialized"));
    host.shutdown(Duration::ZERO).await;
}

#[tokio::test]
async fn spawn_environment_and_cwd_are_resolved_without_secret_results_or_warnings() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("child")).unwrap();
    let mut settings = config(Vec::new());
    settings.cwd = Some("child".into());
    settings.env.insert("PLAIN_VALUE".into(), "ordinary".into());
    settings
        .env_from
        .insert("CHILD_SECRET".into(), "PATH".into());
    let host = start(settings.clone(), root.path()).await;
    assert_eq!(
        invoke(&host, "env", json!({"name":"CHILD_SECRET"}))
            .await
            .content,
        "[redacted]"
    );
    assert_eq!(
        invoke(&host, "env", json!({"name":"PLAIN_VALUE"}))
            .await
            .content,
        "ordinary"
    );
    let cwd = invoke(&host, "cwd", json!({})).await.content;
    assert_eq!(Path::new(&cwd), root.path().join("child"));
    host.shutdown(Duration::ZERO).await;
    settings.args = vec![
        "--stderr-env".into(),
        "CHILD_SECRET".into(),
        "--version".into(),
        "unsupported".into(),
    ];
    let host = start(settings, root.path()).await;
    assert!(host.tools().is_empty());
    assert_eq!(host.warnings().len(), 1);
    assert!(
        host.warnings()[0].contains("[redacted]"),
        "{:?}",
        host.warnings()
    );
    assert!(!host.warnings()[0].contains(&std::env::var("PATH").unwrap()));
    host.shutdown(Duration::ZERO).await;
}

#[cfg(unix)]
fn running(pid: u32) -> bool {
    // A dead grandchild can briefly be a zombie until init reaps it.
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        && stat
            .rsplit_once(") ")
            .is_some_and(|(_, state)| state.starts_with('Z'))
    {
        return false;
    }
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
#[cfg(unix)]
fn pid_file(path: &Path) -> u32 {
    std::fs::read_to_string(path).unwrap().parse().unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_and_drop_kill_owned_unix_process_groups_and_disable_retained_tools() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("parent.pid");
    let child = root.path().join("child.pid");
    let args = vec![
        "--pid-file".into(),
        parent.display().to_string(),
        "--child-pid-file".into(),
        child.display().to_string(),
        "--ignore-eof".into(),
    ];
    let host = start(config(args.clone()), root.path()).await;
    let parent_pid = pid_file(&parent);
    let child_pid = pid_file(&child);
    assert!(running(parent_pid));
    assert!(running(child_pid));
    let retained = tool(&host, "echo");
    host.shutdown(Duration::from_millis(30)).await;
    eventually(|| !running(parent_pid) && !running(child_pid)).await;
    let result = retained
        .invoke(context(), json!({"text":"after shutdown"}))
        .await
        .unwrap();
    assert!(result.is_error);
    assert!(result.content.contains("not sent"));
    let host = start(config(args), root.path()).await;
    let parent_pid = pid_file(&parent);
    let child_pid = pid_file(&child);
    let retained = tool(&host, "echo");
    drop(host);
    eventually(|| !running(parent_pid) && !running(child_pid)).await;
    assert!(
        retained
            .invoke(context(), json!({}))
            .await
            .unwrap()
            .is_error
    );
}

fn settings(config: McpServerSettings) -> McpSettings {
    McpSettings {
        servers: BTreeMap::from([("fixture".into(), config)]),
        ..Default::default()
    }
}
fn write_tools(path: &Path, tools: Value) {
    std::fs::write(path, serde_json::to_vec(&tools).unwrap()).unwrap();
}

#[tokio::test]
async fn reload_add_remove_change_and_force_preserve_unchanged_servers() {
    let root = tempfile::tempdir().unwrap();
    let mut config = settings(config(Vec::new()));
    let host = McpHost::start(&config, root.path()).await;
    let retained = tool(&host, "pid");
    let first = retained.invoke(context(), json!({})).await.unwrap().content;
    assert!(host.reload(&config, None).await.unwrap().is_empty());
    config
        .servers
        .insert("other".into(), config.servers["fixture"].clone());
    let added = host.reload(&config, None).await.unwrap();
    assert_eq!(added.started, ["other"]);
    assert_eq!(added.added.len(), 13);
    assert_eq!(
        retained.invoke(context(), json!({})).await.unwrap().content,
        first
    );
    config.servers.get_mut("other").unwrap().exclude = vec!["echo".into()];
    let changed = host.reload(&config, None).await.unwrap();
    assert!(changed.restarted.is_empty());
    assert_eq!(changed.filtered, ["other"]);
    assert_eq!(changed.removed, ["mcp__other__echo"]);
    assert_eq!(
        retained.invoke(context(), json!({})).await.unwrap().content,
        first
    );
    let forced = host.reload(&config, Some("fixture")).await.unwrap();
    assert_eq!(forced.restarted, ["fixture"]);
    assert!(forced.added.is_empty() && forced.removed.is_empty() && forced.changed.is_empty());
    assert_ne!(
        retained.invoke(context(), json!({})).await.unwrap().content,
        first
    );
    config.servers.remove("fixture");
    let removed = host.reload(&config, None).await.unwrap();
    assert_eq!(removed.stopped, ["fixture"]);
    let result = retained.invoke(context(), json!({})).await.unwrap();
    assert!(
        result.is_error
            && result.content.contains("removed")
            && result.content.contains("not sent")
    );
    assert!(host.reload(&config, Some("missing")).await.is_err());
    host.shutdown(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn reload_validation_failure_preserves_old_generation_and_inventory() {
    let root = tempfile::tempdir().unwrap();
    let mut config = settings(config(Vec::new()));
    let host = McpHost::start(&config, root.path()).await;
    let pid = invoke(&host, "pid", json!({})).await.content;
    let names: Vec<_> = host.tools().iter().map(|t| t.definition().name).collect();
    config.servers.get_mut("fixture").unwrap().url = Some("https://example.invalid/mcp".into());
    assert!(host.reload(&config, None).await.is_err());
    assert_eq!(invoke(&host, "pid", json!({})).await.content, pid);
    assert_eq!(
        host.tools()
            .iter()
            .map(|t| t.definition().name)
            .collect::<Vec<_>>(),
        names
    );
    host.shutdown(Duration::ZERO).await;
}

#[tokio::test]
async fn filters_apply_to_discovery_notifications_and_live_reload_without_reconnect() {
    let root = tempfile::tempdir().unwrap();
    let tools_file = root.path().join("tools.json");
    let log = root.path().join("requests.jsonl");
    write_tools(
        &tools_file,
        json!([
            {"name":"echo","inputSchema":{"type":"object"}},
            {"name":"read_note","inputSchema":{"type":"object"}},
            {"name":"read_secret","inputSchema":{"type":"object"}},
            {"name":"other","inputSchema":{"type":"object"}}
        ]),
    );
    let mut config = settings(config(vec![
        "--tools-file".into(),
        tools_file.display().to_string(),
        "--log".into(),
        log.display().to_string(),
    ]));
    config.servers.get_mut("fixture").unwrap().include_tools =
        Some(vec!["echo".into(), "read_*".into()]);
    config.servers.get_mut("fixture").unwrap().exclude_tools = vec!["*_secret".into()];
    let host = McpHost::start(&config, root.path()).await;
    let names = |host: &McpHost| {
        host.tools()
            .iter()
            .map(|tool| tool.definition().name)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(&host),
        ["mcp__fixture__echo", "mcp__fixture__read_note"]
    );
    let retained = tool(&host, "echo");
    write_tools(
        &tools_file,
        json!([
            {"name":"echo","inputSchema":{"type":"object"}},
            {"name":"read_new","inputSchema":{"type":"object"}},
            {"name":"read_secret","inputSchema":{"type":"object"}},
            {"name":"other","inputSchema":{"type":"object"}}
        ]),
    );
    invoke(&host, "echo", json!({"notify_tools_changed":true})).await;
    let refreshed = host.refresh_tools().await;
    assert_eq!(refreshed.added, ["mcp__fixture__read_new"]);
    assert_eq!(refreshed.removed, ["mcp__fixture__read_note"]);
    assert_eq!(
        names(&host),
        ["mcp__fixture__echo", "mcp__fixture__read_new"]
    );
    let removed = tool(&host, "read_new");
    config.servers.get_mut("fixture").unwrap().include_tools =
        Some(vec!["echo".into(), "other".into()]);
    let changed = host.reload(&config, None).await.unwrap();
    assert_eq!(changed.filtered, ["fixture"]);
    assert!(changed.restarted.is_empty());
    assert_eq!(changed.added, ["mcp__fixture__other"]);
    assert_eq!(changed.removed, ["mcp__fixture__read_new"]);
    assert!(
        removed
            .invoke(context(), json!({}))
            .await
            .unwrap()
            .content
            .contains("not sent")
    );
    assert_eq!(
        retained
            .invoke(context(), json!({"text":"retained"}))
            .await
            .unwrap()
            .content,
        "retained"
    );
    assert_eq!(names(&host), ["mcp__fixture__echo", "mcp__fixture__other"]);
    // Later notifications keep the live filters rather than the startup filters.
    invoke(&host, "echo", json!({"notify_tools_changed":true})).await;
    assert!(host.refresh_tools().await.is_empty());
    assert_eq!(names(&host), ["mcp__fixture__echo", "mcp__fixture__other"]);
    // Even a filter edit that leaves the visible tools unchanged is reported.
    config
        .servers
        .get_mut("fixture")
        .unwrap()
        .exclude_tools
        .push("missing".into());
    let unchanged_inventory = host.reload(&config, None).await.unwrap();
    assert_eq!(unchanged_inventory.filtered, ["fixture"]);
    assert!(!unchanged_inventory.is_empty());
    assert!(unchanged_inventory.added.is_empty() && unchanged_inventory.removed.is_empty());
    assert!(host.reload(&config, None).await.unwrap().is_empty());
    config.servers.get_mut("fixture").unwrap().include_tools = Some(Vec::new());
    let empty = host.reload(&config, None).await.unwrap();
    assert_eq!(empty.removed, ["mcp__fixture__echo", "mcp__fixture__other"]);
    assert!(host.tools().is_empty());
    assert!(
        retained
            .invoke(context(), json!({}))
            .await
            .unwrap()
            .content
            .contains("not sent")
    );
    // Removing the new allowlist restores unrestricted discovery from the raw list.
    config.servers.get_mut("fixture").unwrap().include_tools = None;
    let restored = host.reload(&config, None).await.unwrap();
    assert_eq!(
        restored.added,
        [
            "mcp__fixture__echo",
            "mcp__fixture__other",
            "mcp__fixture__read_new"
        ]
    );
    let requests = requests(&log);
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["request"]["method"] == "initialize")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["request"]["method"] == "tools/list")
            .count(),
        3
    );
    assert!(
        !requests
            .iter()
            .any(|r| r["request"]["params"]["name"] == "read_new")
    );
    host.shutdown(Duration::ZERO).await;
}

#[tokio::test]
async fn stdio_notification_refreshes_validated_tools_without_restarting() {
    let root = tempfile::tempdir().unwrap();
    let tools = root.path().join("tools.json");
    write_tools(
        &tools,
        json!([
            {"name":"echo","description":"old echo","inputSchema":{"type":"object"}},
            {"name":"pid","inputSchema":{"type":"object"}}
        ]),
    );
    let host = start(
        config(vec!["--tools-file".into(), tools.display().to_string()]),
        root.path(),
    )
    .await;
    let old_echo = tool(&host, "echo");
    let old_pid = tool(&host, "pid");
    let pid = old_pid.invoke(context(), json!({})).await.unwrap().content;
    assert!(host.refresh_tools().await.is_empty());
    write_tools(
        &tools,
        json!([
            {"name":"echo","description":"new echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}},
            {"name":"added","inputSchema":{"type":"object"}},
            {"name":"bad","inputSchema":{"type":17}}
        ]),
    );
    old_echo
        .invoke(context(), json!({"notify_tools_changed":true}))
        .await
        .unwrap();
    let diff = host.refresh_tools().await;
    assert_eq!(diff.added, ["mcp__fixture__added"]);
    assert_eq!(diff.removed, ["mcp__fixture__pid"]);
    assert_eq!(diff.changed, ["mcp__fixture__echo"]);
    assert!(
        diff.warnings
            .iter()
            .any(|w| w.contains("invalid input schema"))
    );
    assert_eq!(
        host.tools()
            .iter()
            .map(|t| t.definition().name)
            .collect::<Vec<_>>(),
        ["mcp__fixture__added", "mcp__fixture__echo"]
    );
    assert!(
        old_pid
            .invoke(context(), json!({}))
            .await
            .unwrap()
            .content
            .contains("removed")
    );
    assert!(
        old_echo
            .invoke(context(), json!({}))
            .await
            .unwrap()
            .content
            .contains("definition changed")
    );
    assert_eq!(
        invoke(&host, "echo", json!({"text":"new schema"}))
            .await
            .content,
        "new schema"
    );
    // Restoring the exact pid definition verifies the connection was retained.
    write_tools(
        &tools,
        json!([
            {"name":"echo","description":"new echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}},
            {"name":"pid","inputSchema":{"type":"object"}}
        ]),
    );
    invoke(&host, "echo", json!({"notify_tools_changed":true})).await;
    host.refresh_tools().await;
    assert_eq!(invoke(&host, "pid", json!({})).await.content, pid);
    assert!(host.refresh_tools().await.is_empty());
    host.shutdown(Duration::ZERO).await;
}

#[tokio::test]
async fn in_flight_call_survives_other_session_reload_and_generation_drains() {
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("requests.jsonl");
    let mut config = settings(config(vec!["--log".into(), log.display().to_string()]));
    config.servers.get_mut("fixture").unwrap().call_timeout_secs = 5;
    let host = Arc::new(McpHost::start(&config, root.path()).await);
    let old_pid: u32 = invoke(&host, "pid", json!({}))
        .await
        .content
        .parse()
        .unwrap();
    let slow = tool(&host, "slow");
    let pending =
        tokio::spawn(async move { slow.invoke(context(), json!({"ms":400})).await.unwrap() });
    eventually(|| {
        requests(&log)
            .iter()
            .any(|r| r["request"]["params"]["name"] == "slow")
    })
    .await;
    let other_session = host.clone();
    other_session
        .reload(&config, Some("fixture"))
        .await
        .unwrap();
    let new_pid: u32 = invoke(&host, "pid", json!({}))
        .await
        .content
        .parse()
        .unwrap();
    assert_ne!(new_pid, old_pid);
    #[cfg(unix)]
    assert!(running(old_pid));
    let result = pending.await.unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "slept 400");
    #[cfg(unix)]
    eventually(|| !running(old_pid)).await;
    assert_eq!(
        requests(&log)
            .iter()
            .filter(|r| r["request"]["params"]["name"] == "slow")
            .count(),
        1
    );
    host.shutdown(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn cancelling_reload_keeps_current_generation_and_stops_unpublished_candidate() {
    let root = tempfile::tempdir().unwrap();
    let candidate_log = root.path().join("candidate.jsonl");
    let mut config = settings(config(Vec::new()));
    let host = Arc::new(McpHost::start(&config, root.path()).await);
    let pid = invoke(&host, "pid", json!({})).await.content;
    config.servers.insert(
        "candidate".into(),
        McpServerSettings {
            args: vec![
                "--initialize-delay-ms".into(),
                "2000".into(),
                "--log".into(),
                candidate_log.display().to_string(),
            ],
            ..config.servers["fixture"].clone()
        },
    );
    let reloading = host.clone();
    let pending = tokio::spawn(async move { reloading.reload(&config, None).await });
    eventually(|| {
        requests(&candidate_log)
            .iter()
            .any(|r| r["request"]["method"] == "initialize")
    })
    .await;
    #[cfg(unix)]
    let candidate_pid = requests(&candidate_log)[0]["pid"].as_u64().unwrap() as u32;
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert_eq!(invoke(&host, "pid", json!({})).await.content, pid);
    assert!(
        host.tools()
            .iter()
            .all(|t| !t.definition().name.contains("candidate"))
    );
    #[cfg(unix)]
    eventually(|| !running(candidate_pid)).await;
    host.shutdown(Duration::ZERO).await;
}

async fn start_with_client(
    config: McpServerSettings,
    workspace: &Path,
    client_env: McpClientEnv,
) -> McpHost {
    McpHost::start_with_client_env(
        &McpSettings {
            servers: BTreeMap::from([("fixture".into(), config)]),
            ..Default::default()
        },
        workspace,
        client_env,
    )
    .await
}

#[tokio::test]
async fn client_env_overlays_stdio_child_and_env_from_resolves_only_from_the_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let mut settings = config(Vec::new());
    settings.env.insert("PLAIN_VALUE".into(), "ordinary".into());
    settings
        .env
        .insert("CLIENT_OVERRIDDEN".into(), "config-wins".into());
    settings
        .env_from
        .insert("CHILD_SECRET".into(), "CLIENT_SECRET".into());
    let client = McpClientEnv::new([
        ("CLIENT_SECRET", "client-secret-value"),
        ("CLIENT_FORWARDED", "forwarded-value"),
        ("CLIENT_OVERRIDDEN", "forwarded-loses"),
    ]);
    assert!(!format!("{client:?}").contains("client-secret-value"));
    let host = start_with_client(settings, root.path(), client).await;
    assert!(host.warnings().is_empty(), "{:?}", host.warnings());
    // Snapshot values are secrets, so a "[redacted]" result also proves the child received them.
    assert_eq!(
        invoke(&host, "env", json!({"name":"CHILD_SECRET"}))
            .await
            .content,
        "[redacted]"
    );
    // A snapshot entry this server's env_from does not name never reaches it.
    assert_eq!(
        invoke(&host, "env", json!({"name":"CLIENT_FORWARDED"}))
            .await
            .content,
        ""
    );
    // Config env is not displaced by a snapshot entry of the same name.
    assert_eq!(
        invoke(&host, "env", json!({"name":"CLIENT_OVERRIDDEN"}))
            .await
            .content,
        "config-wins"
    );
    assert_eq!(
        invoke(&host, "env", json!({"name":"PLAIN_VALUE"}))
            .await
            .content,
        "ordinary"
    );
    host.shutdown(Duration::ZERO).await;
}

#[tokio::test]
async fn client_env_never_falls_back_to_host_env_for_env_from() {
    let root = tempfile::tempdir().unwrap();
    let mut settings = config(Vec::new());
    // PATH is always set in the test process, so only a snapshot miss can fail here.
    settings
        .env_from
        .insert("CHILD_SECRET".into(), "PATH".into());
    let host = start_with_client(
        settings,
        root.path(),
        McpClientEnv::new([("UNRELATED_CLIENT_VALUE", "unused")]),
    )
    .await;
    assert!(host.tools().is_empty());
    let warning = host.warnings().join("\n");
    assert!(
        warning.contains("required host environment variable is missing"),
        "{warning}"
    );
    host.shutdown(Duration::ZERO).await;
}

#[tokio::test]
async fn client_snapshot_survives_forced_reload_and_crash_reconnect() {
    let root = tempfile::tempdir().unwrap();
    let mut server = config(Vec::new());
    server
        .env_from
        .insert("CHILD_SECRET".into(), "CLIENT_SECRET".into());
    let settings = McpSettings {
        servers: BTreeMap::from([("fixture".into(), server)]),
        ..Default::default()
    };
    let host = McpHost::start_with_client_env(
        &settings,
        root.path(),
        McpClientEnv::new([("CLIENT_SECRET", "client-secret-value")]),
    )
    .await;
    assert!(host.warnings().is_empty(), "{:?}", host.warnings());
    let first_pid = invoke(&host, "pid", json!({})).await.content;
    assert_eq!(child_secret(&host).await, "[redacted]");

    // A forced reload starts a new child from the same settings and the same snapshot.
    let diff = host.reload(&settings, Some("fixture")).await.unwrap();
    assert_eq!(diff.restarted, ["fixture"]);
    let reloaded_pid = invoke(&host, "pid", json!({})).await.content;
    assert_ne!(first_pid, reloaded_pid);
    assert_eq!(child_secret(&host).await, "[redacted]");

    // A crash fails the pending call; the next call reconnects from the retained snapshot.
    assert!(invoke(&host, "crash", json!({})).await.is_error);
    let reconnected_pid = invoke(&host, "pid", json!({})).await.content;
    assert_ne!(reloaded_pid, reconnected_pid);
    assert_eq!(child_secret(&host).await, "[redacted]");
    host.shutdown(Duration::ZERO).await;
}

/// Redacted when the child received the snapshot value through `env_from`.
async fn child_secret(host: &McpHost) -> String {
    invoke(host, "env", json!({"name":"CHILD_SECRET"}))
        .await
        .content
}

#[tokio::test]
async fn client_value_reaches_only_the_server_whose_env_from_names_it() {
    let root = tempfile::tempdir().unwrap();
    let mut a = config(Vec::new());
    a.env_from
        .insert("GITHUB_TOKEN".into(), "CLIENT_GITHUB_TOKEN".into());
    let b = config(Vec::new());
    let host = McpHost::start_with_client_env(
        &McpSettings {
            servers: BTreeMap::from([("a".into(), a), ("b".into(), b)]),
            ..Default::default()
        },
        root.path(),
        McpClientEnv::new([("CLIENT_GITHUB_TOKEN", "client-github-value")]),
    )
    .await;
    assert!(host.warnings().is_empty(), "{:?}", host.warnings());
    let env = |server: &str| {
        let name = format!("mcp__{server}__env");
        host.tools()
            .into_iter()
            .find(|tool| tool.definition().name == name)
            .unwrap_or_else(|| panic!("missing {name}"))
    };
    let read = |server: &str, name: &str| {
        let tool = env(server);
        let input = json!({ "name": name });
        async move { tool.invoke(context(), input).await.unwrap().content }
    };
    // Server A receives the value through its own env_from (redacted in results).
    assert_eq!(read("a", "GITHUB_TOKEN").await, "[redacted]");
    // Server B has no env_from: neither the child name nor the snapshot name is set.
    assert_eq!(read("b", "GITHUB_TOKEN").await, "");
    assert_eq!(read("b", "CLIENT_GITHUB_TOKEN").await, "");
    host.shutdown(Duration::ZERO).await;
}
