//! Scripted host-only coverage: real local commands, no model/provider calls.
use bubblepaw_core::{CancellationToken, ToolContext, ToolRegistry};
use bubblepaw_runner::{
    config::ExecConfig,
    tools::{ProcessTools, WorkspaceTools},
};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};

#[path = "support/notice_provider.rs"]
mod notice_provider;
use notice_provider::{NoticeProvider, chat_measure};

struct Fixture {
    _workspace: tempfile::TempDir,
    data: tempfile::TempDir,
    processes: Arc<ProcessTools>,
    registry: ToolRegistry,
}
impl Fixture {
    fn new(config: ExecConfig) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let processes = Arc::new(
            ProcessTools::with_config(
                WorkspaceTools::new(workspace.path()).unwrap(),
                config,
                data.path(),
            )
            .unwrap(),
        );
        let mut registry = ToolRegistry::new();
        processes.register(&mut registry).unwrap();
        processes.register_wait(&mut registry).unwrap();
        Self {
            _workspace: workspace,
            data,
            processes,
            registry,
        }
    }
}
fn context(owner: &str) -> ToolContext {
    ToolContext {
        session_id: owner.into(),
        run_id: "run".into(),
        invocation_id: "invocation".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    }
}
async fn call(registry: &ToolRegistry, owner: &str, name: &str, input: Value) -> Value {
    let result = registry
        .get(name)
        .unwrap()
        .invoke(context(owner), input)
        .await
        .unwrap();
    assert!(!result.is_error);
    serde_json::from_str(&result.content).unwrap()
}
#[cfg(windows)]
const ECHO: &str = "[Console]::Out.Write('hello'); [Console]::Error.Write('problem')";
#[cfg(not(windows))]
const ECHO: &str = "printf hello; printf problem >&2";
#[cfg(windows)]
const DELAYED: &str =
    "[Console]::Out.Write('first'); Start-Sleep -Milliseconds 1800; [Console]::Out.Write('last')";
#[cfg(not(windows))]
const DELAYED: &str = "printf first; sleep 1.8; printf last";
#[cfg(windows)]
const SLEEP: &str = "Start-Sleep -Seconds 30";
#[cfg(not(windows))]
const SLEEP: &str = "sleep 30";

async fn foreground(processes: &ProcessTools, owner: &str) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let jobs = processes.jobs(owner).unwrap();
            if let Some(job) = jobs["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|job| job["foreground"] == true)
            {
                return job["handle"].as_str().unwrap().to_owned();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
fn spool_size(root: &Path) -> u64 {
    std::fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                spool_size(&entry.path())
            } else {
                // Windows directory entries can retain size zero while the
                // spool's write handle is open. Query the file itself.
                std::fs::metadata(entry.path()).unwrap().len()
            }
        })
        .sum()
}

#[tokio::test]
async fn sync_default_returns_complete_output_and_poll_consumes_server_cursor() {
    let fixture = Fixture::new(ExecConfig::default());
    let result = call(&fixture.registry, "owner", "exec", json!({"command":ECHO})).await;
    assert_eq!(result["status"], "exited");
    assert_eq!(result["exitCode"], 0);
    assert_eq!(result["stdout"], "hello");
    assert_eq!(result["stderr"], "problem");
    assert!(fixture.processes.take_reminders("owner").is_empty());
    let next = call(
        &fixture.registry,
        "owner",
        "poll",
        json!({"handle":result["handle"]}),
    )
    .await;
    assert_eq!(next["stdout"], "hello");
    let consumed = call(
        &fixture.registry,
        "owner",
        "poll",
        json!({"handle":result["handle"]}),
    )
    .await;
    assert_eq!(consumed["stdout"], "");
    assert_eq!(consumed["stderr"], "");
    let replay = call(
        &fixture.registry,
        "owner",
        "poll",
        json!({"handle":result["handle"],"cursor":{"stdout":0,"stderr":0}}),
    )
    .await;
    assert_eq!(replay["stdout"], "hello");
    assert_eq!(replay["stderr"], "problem");
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn twelve_kib_per_stream_is_returned_without_preview_gaps() {
    let fixture = Fixture::new(ExecConfig::default());
    #[cfg(windows)]
    let command = "[Console]::Out.Write(('x' * 12288)); [Console]::Error.Write(('y' * 12288))";
    #[cfg(not(windows))]
    let command =
        r"head -c 12288 /dev/zero | tr '\000' x; head -c 12288 /dev/zero | tr '\000' y >&2";
    let result = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command":command}),
    )
    .await;
    assert_eq!(result["status"], "exited");
    assert_eq!(result["stdout"], "x".repeat(12 * 1024));
    assert_eq!(result["stderr"], "y".repeat(12 * 1024));
    assert_eq!(result["capture"]["stdout"]["gaps"], json!([]));
    assert_eq!(result["capture"]["stderr"]["gaps"], json!([]));
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn default_process_wait_waits_for_five_second_command_to_exit() {
    let fixture = Fixture::new(ExecConfig::default());
    #[cfg(windows)]
    let command = "Start-Sleep -Seconds 5";
    #[cfg(not(windows))]
    let command = "sleep 5";
    let started = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command":command,"background":true}),
    )
    .await;
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        call(
            &fixture.registry,
            "owner",
            "wait",
            json!({"handle":started["handle"]}),
        ),
    )
    .await
    .unwrap();
    assert_eq!(result["status"], "exited");
    assert_eq!(result["exitCode"], 0);
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn explicit_exec_waits_can_exceed_the_configured_sync_timeout() {
    let fixture = Fixture::new(ExecConfig {
        sync_timeout_secs: 1,
        ..ExecConfig::default()
    });
    for options in [json!({"timeoutMs":5000}), json!({"yieldMs":5000})] {
        let mut input = options;
        input["command"] = json!(DELAYED);
        let result = call(&fixture.registry, "owner", "exec", input).await;
        assert_eq!(result["status"], "exited", "{result}");
        assert_eq!(result["stdout"], "firstlast");
        assert_eq!(result["exitCode"], 0);
        assert!(result.get("reason").is_none());
    }
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn large_explicit_exec_waits_are_accepted_even_with_a_zero_sync_default() {
    let fixture = Fixture::new(ExecConfig {
        sync_timeout_secs: 0,
        ..ExecConfig::default()
    });
    for options in [json!({"timeoutMs":900001}), json!({"yieldMs":900001})] {
        let mut input = options;
        input["command"] = json!(ECHO);
        let result = call(&fixture.registry, "owner", "exec", input).await;
        assert_eq!(result["status"], "exited", "{result}");
        assert_eq!(result["stdout"], "hello");
    }
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn workdir_alias_matches_cwd_and_rejects_conflicting_directories() {
    let fixture = Fixture::new(ExecConfig::default());
    let subdirectory = fixture._workspace.path().join("sub");
    std::fs::create_dir(&subdirectory).unwrap();
    std::fs::write(subdirectory.join("marker.txt"), "from subdirectory").unwrap();
    #[cfg(windows)]
    let command = "[Console]::Out.Write([IO.File]::ReadAllText('marker.txt'))";
    #[cfg(not(windows))]
    let command = "cat marker.txt";
    for options in [
        json!({"workdir":"sub"}),
        json!({"cwd":"sub"}),
        json!({"cwd":"sub","workdir":"./sub"}),
    ] {
        let mut input = options;
        input["command"] = json!(command);
        let result = call(&fixture.registry, "owner", "exec", input).await;
        assert_eq!(result["status"], "exited", "{result}");
        assert_eq!(result["stdout"], "from subdirectory");
    }
    let tool = fixture.registry.get("exec").unwrap();
    assert_eq!(
        tool.definition().input_schema["properties"]["workdir"]["type"],
        "string"
    );
    let error = tool
        .invoke(
            context("owner"),
            json!({"command":ECHO,"cwd":".","workdir":"sub"}),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cwd and workdir"), "{error}");
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn bare_uuid_process_handles_support_poll_wait_cancel_and_keep_owner_isolation() {
    let fixture = Fixture::new(ExecConfig::default());
    let started = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command":SLEEP,"background":true}),
    )
    .await;
    let handle = started["handle"].as_str().unwrap();
    let bare = handle.strip_prefix("process:").unwrap();
    for name in ["poll", "wait", "cancel"] {
        let error = fixture
            .registry
            .get(name)
            .unwrap()
            .invoke(context("other"), json!({"handle":bare}))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("another conversation"),
            "{error}"
        );
    }
    let polled = call(&fixture.registry, "owner", "poll", json!({"handle":bare})).await;
    assert_eq!(polled["status"], "running");
    assert_eq!(polled["handle"], handle);
    let waited = call(
        &fixture.registry,
        "owner",
        "wait",
        json!({"handle":bare,"timeoutMs":0}),
    )
    .await;
    assert_eq!(waited["status"], "timeout");
    assert_eq!(waited["handle"], handle);
    assert_eq!(
        call(&fixture.registry, "owner", "poll", json!({"handle":bare})).await["status"],
        "running"
    );
    assert_eq!(
        call(&fixture.registry, "owner", "cancel", json!({"handle":bare})).await["status"],
        "cancelled"
    );
    assert_eq!(
        call(&fixture.registry, "owner", "wait", json!({"handle":bare})).await["status"],
        "cancelled"
    );
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn configured_sync_timeout_backgrounds_without_killing_and_notifies_once() {
    let fixture = Fixture::new(ExecConfig {
        sync_timeout_secs: 1,
        ..ExecConfig::default()
    });
    let result = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command":DELAYED}),
    )
    .await;
    assert_eq!(result["status"], "backgrounded");
    assert_eq!(result["reason"], "sync_timeout");
    let jobs = call(&fixture.registry, "owner", "jobs", json!({})).await;
    assert_eq!(jobs["jobs"][0]["invocationId"], "invocation");
    assert_eq!(jobs["jobs"][0]["status"], "running");
    assert!(
        fixture.processes.jobs("other").unwrap()["jobs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let done = call(
        &fixture.registry,
        "owner",
        "wait",
        json!({"handle":result["handle"],"timeoutMs":5000}),
    )
    .await;
    assert_eq!(done["status"], "exited");
    assert_eq!(done["exitCode"], 0);
    let next = call(
        &fixture.registry,
        "owner",
        "poll",
        json!({"handle":result["handle"]}),
    )
    .await;
    assert_eq!(next["stdout"], "firstlast");
    let exec = fixture.registry.get("exec").unwrap();
    let notice = exec.take_reminders("owner");
    assert_eq!(notice.len(), 1);
    assert!(notice[0].text.contains(result["handle"].as_str().unwrap()));
    assert!(notice[0].text.contains("stdout 9 bytes"));
    let poll = fixture.registry.get("poll").unwrap();
    assert!(poll.take_reminders("owner").is_empty());
    assert!(fixture.processes.take_reminders("owner").is_empty());
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn explicit_background_zero_sync_timeout_and_hard_deadline_are_distinct() {
    let fixture = Fixture::new(ExecConfig {
        sync_timeout_secs: 0,
        ..ExecConfig::default()
    });
    let result = call(&fixture.registry, "owner", "exec", json!({"command":SLEEP})).await;
    assert_eq!(result["status"], "backgrounded");
    assert_eq!(result["reason"], "sync_timeout");
    call(
        &fixture.registry,
        "owner",
        "cancel",
        json!({"handle":result["handle"]}),
    )
    .await;
    let result = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command":SLEEP,"background":true,"timeoutMs":100}),
    )
    .await;
    assert_eq!(result["status"], "running");
    assert!(result.get("reason").is_none());
    let done = call(
        &fixture.registry,
        "owner",
        "wait",
        json!({"handle":result["handle"],"timeoutMs":5000}),
    )
    .await;
    assert_eq!(done["status"], "timed_out");
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn user_background_and_steer_release_only_the_owned_sync_wait() {
    let fixture = Fixture::new(ExecConfig::default());
    for reason in ["user", "steer"] {
        let tool = fixture.registry.get("exec").unwrap();
        let pending = tokio::spawn(async move {
            tool.invoke(context("owner"), json!({"command":SLEEP}))
                .await
        });
        let handle = foreground(&fixture.processes, "owner").await;
        assert!(fixture.processes.background("other", &handle).is_err());
        assert_eq!(fixture.processes.interrupt_waits("other"), 0);
        if reason == "user" {
            let result = fixture.processes.background("owner", &handle).unwrap();
            assert_eq!(result["reason"], "user");
        } else {
            assert_eq!(
                fixture
                    .registry
                    .get("exec")
                    .unwrap()
                    .interrupt_wait("owner"),
                1
            );
            assert_eq!(fixture.processes.interrupt_waits("owner"), 0);
        }
        let result: Value = serde_json::from_str(
            &tokio::time::timeout(Duration::from_secs(3), pending)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .content,
        )
        .unwrap();
        assert_eq!(result["status"], "backgrounded");
        assert_eq!(result["reason"], reason);
        assert_eq!(result["handle"], handle);
        assert_eq!(
            fixture.processes.jobs("owner").unwrap()["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|job| job["handle"] == handle)
                .unwrap()["status"],
            "running"
        );
        call(
            &fixture.registry,
            "owner",
            "cancel",
            json!({"handle":handle}),
        )
        .await;
    }
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn dropped_foreground_wait_keeps_job_observable_and_delivers_completion_notice() {
    let fixture = Fixture::new(ExecConfig::default());
    let tool = fixture.registry.get("exec").unwrap();
    let pending = tokio::spawn(async move {
        tool.invoke(context("owner"), json!({"command":DELAYED}))
            .await
    });
    let handle = foreground(&fixture.processes, "owner").await;
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert_eq!(
        fixture.processes.jobs("owner").unwrap()["jobs"][0]["backgrounded"],
        true
    );
    let done = call(
        &fixture.registry,
        "owner",
        "wait",
        json!({"handle":handle,"timeoutMs":5000}),
    )
    .await;
    assert_eq!(done["status"], "exited");
    assert_eq!(fixture.processes.take_reminders("owner").len(), 1);
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn optional_caps_are_atomic_at_spawn_and_completed_jobs_release_capacity() {
    let fixture = Fixture::new(ExecConfig {
        max_jobs_per_session: Some(1),
        max_jobs: Some(2),
        ..ExecConfig::default()
    });
    let alice = call(
        &fixture.registry,
        "alice",
        "exec",
        json!({"command":SLEEP,"background":true}),
    )
    .await;
    let error = fixture
        .registry
        .get("exec")
        .unwrap()
        .invoke(context("alice"), json!({"command":ECHO,"background":true}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("max_jobs_per_session"));
    call(
        &fixture.registry,
        "bob",
        "exec",
        json!({"command":SLEEP,"background":true}),
    )
    .await;
    let error = fixture
        .registry
        .get("exec")
        .unwrap()
        .invoke(context("carol"), json!({"command":ECHO,"background":true}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("max_jobs limit"));
    call(
        &fixture.registry,
        "alice",
        "cancel",
        json!({"handle":alice["handle"]}),
    )
    .await;
    assert_eq!(
        call(&fixture.registry, "alice", "exec", json!({"command":ECHO})).await["status"],
        "exited"
    );
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn unset_caps_accept_more_than_the_previous_32_live_jobs() {
    let fixture = Fixture::new(ExecConfig::default());
    for _ in 0..33 {
        call(
            &fixture.registry,
            "owner",
            "exec",
            json!({"command":SLEEP,"background":true}),
        )
        .await;
    }
    assert_eq!(
        fixture.processes.jobs("owner").unwrap()["jobs"]
            .as_array()
            .unwrap()
            .len(),
        33
    );
    fixture.processes.cancel_owner("owner").await;
    assert!(
        fixture.processes.jobs("owner").unwrap()["jobs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(spool_size(&fixture.data.path().join("jobs")), 0);
}

#[tokio::test]
async fn spool_is_capped_across_streams_and_removed_on_reap_and_restart() {
    let fixture = Fixture::new(ExecConfig {
        max_output_bytes: 12_345,
        ..ExecConfig::default()
    });
    #[cfg(windows)]
    let chatty = "[Console]::Out.Write(('a' * 90000)); [Console]::Error.Write(('b' * 90000))";
    #[cfg(not(windows))]
    let chatty =
        r"head -c 90000 /dev/zero | tr '\000' a; head -c 90000 /dev/zero | tr '\000' b >&2";
    let result = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command":chatty,"background":true}),
    )
    .await;
    let done = call(
        &fixture.registry,
        "owner",
        "wait",
        json!({"handle":result["handle"],"timeoutMs":5000}),
    )
    .await;
    assert_eq!(done["status"], "exited");
    assert_eq!(spool_size(&fixture.data.path().join("jobs")), 12_345);
    let job = &fixture.processes.jobs("owner").unwrap()["jobs"][0];
    assert_eq!(job["stdoutBytes"], 90_000);
    assert_eq!(job["stderrBytes"], 90_000);
    assert_eq!(job["spooledBytes"], 12_345);
    let poll = call(
        &fixture.registry,
        "owner",
        "poll",
        json!({"handle":result["handle"],"cursor":{"stdout":0,"stderr":0}}),
    )
    .await;
    assert!(poll["stdout"].as_str().unwrap().len() <= 16 * 1024);
    assert!(poll["stdout"].as_str().unwrap().contains("omitted bytes"));
    assert!(
        !poll["capture"]["stdout"]["spoolGaps"]
            .as_array()
            .unwrap()
            .is_empty()
            || !poll["capture"]["stderr"]["spoolGaps"]
                .as_array()
                .unwrap()
                .is_empty()
    );
    assert_eq!(spool_size(&fixture.data.path().join("jobs")), 0);
    fixture.processes.shutdown().await;
    std::fs::create_dir_all(fixture.data.path().join("jobs/stale")).unwrap();
    std::fs::write(
        fixture.data.path().join("jobs/stale/old.out"),
        "stale output",
    )
    .unwrap();
    let restarted = ProcessTools::with_config(
        WorkspaceTools::new(fixture._workspace.path()).unwrap(),
        ExecConfig::default(),
        fixture.data.path(),
    )
    .unwrap();
    assert_eq!(spool_size(&fixture.data.path().join("jobs")), 0);
    assert!(
        restarted.jobs("owner").unwrap()["jobs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    restarted.shutdown().await;
}

#[tokio::test]
async fn idle_completion_notice_is_durable_once_and_chatty_output_has_no_chunk_transactions() {
    use bubblepaw_core::{Harness, HarnessConfig, MessageOrigin, RunOptions, Store};
    use bubblepaw_runner::{artifacts::ArtifactStore, codemode::CodeMode};
    let fixture = Fixture::new(ExecConfig {
        max_output_bytes: 65_536,
        ..ExecConfig::default()
    });
    let store = Store::open_data_dir(fixture.data.path()).unwrap();
    let session = store.create_session("chatty process").unwrap();
    let mut registry = fixture.registry.clone();
    let repl = Arc::new(
        CodeMode::new(
            fixture._workspace.path().to_owned(),
            Arc::new(
                ArtifactStore::new(
                    fixture._workspace.path(),
                    fixture._workspace.path().join("artifacts"),
                )
                .unwrap(),
            ),
        )
        .unwrap(),
    );
    repl.register(&mut registry).unwrap();
    #[cfg(windows)]
    let command = "Start-Sleep -Seconds 1; [Console]::Out.Write(('a' * 2000000)); [Console]::Error.Write(('b' * 2000000))";
    #[cfg(not(windows))]
    let command = r"sleep 1; head -c 2000000 /dev/zero | tr '\000' a; head -c 2000000 /dev/zero | tr '\000' b >&2";
    let provider = Arc::new(NoticeProvider {
        code: format!(
            "const job = await tools.exec({}); return {{handle:job.handle,status:job.status}};",
            json!({"command":command,"background":true})
        ),
        requests: Default::default(),
    });
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        registry.clone(),
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    );
    let run = harness
        .prompt(&session.id, "launch background job", RunOptions::default())
        .await
        .unwrap();
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    let jobs = fixture.processes.jobs(&session.id).unwrap();
    let handle = jobs["jobs"][0]["handle"].as_str().unwrap().to_owned();
    assert_eq!(jobs["jobs"][0]["status"], "running");
    let before = store.storage_stats().unwrap();
    let chat_path = fixture
        .data
        .path()
        .join("chats")
        .join(format!("{}.db", session.id));
    let chat_before = chat_measure(&chat_path);
    let done = call(
        &registry,
        &session.id,
        "wait",
        json!({"handle":handle,"timeoutMs":10000}),
    )
    .await;
    assert_eq!(done["status"], "exited");
    tokio::time::timeout(Duration::from_secs(3), async {
        while !store.pending_effects(&session.id).unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let after = store.storage_stats().unwrap();
    let chat_after = chat_measure(&chat_path);
    assert!(
        chat_before.1 >= 1,
        "starting an exec must keep an effect boundary"
    );
    assert_eq!(
        chat_after.0, chat_before.0,
        "pipe chunks must not become chat rows"
    );
    assert_eq!(
        chat_after.1, chat_before.1,
        "pipe chunks must not become effect rows"
    );
    assert_eq!(
        after.message_count, before.message_count,
        "output chunks must not become messages"
    );
    assert!(
        after
            .transaction_count
            .saturating_sub(before.transaction_count)
            <= 3,
        "only the effect completion boundary may write while idle"
    );
    assert!(after.database_bytes.saturating_sub(before.database_bytes) <= 128 * 1024);
    assert!(after.wal_bytes.saturating_sub(before.wal_bytes) <= 256 * 1024);
    eprintln!(
        "chatty job 4000000 bytes: message rows {}->{}, transactions {}->{}, SQLite bytes {}->{}, WAL bytes {}->{}",
        before.message_count,
        after.message_count,
        before.transaction_count,
        after.transaction_count,
        before.database_bytes,
        after.database_bytes,
        before.wal_bytes,
        after.wal_bytes
    );
    eprintln!(
        "chat file: messages {}->{}, effects {}->{}, bytes {}->{}, WAL bytes {}->{}",
        chat_before.0,
        chat_after.0,
        chat_before.1,
        chat_after.1,
        chat_before.2,
        chat_after.2,
        chat_before.3,
        chat_after.3
    );
    assert_eq!(spool_size(&fixture.data.path().join("jobs")), 65_536);
    assert_eq!(
        provider.requests.lock().unwrap().len(),
        2,
        "completion must not wake an idle session"
    );
    assert!(
        !store
            .messages(&session.id)
            .unwrap()
            .iter()
            .any(|message| message.content.starts_with("<harness "))
    );
    assert_eq!(
        harness
            .prompt(&session.id, "check finished job", RunOptions::default())
            .await
            .unwrap()
            .status,
        "completed"
    );
    let messages = store.messages(&session.id).unwrap();
    let notices = messages
        .iter()
        .filter(|message| message.content.starts_with("<harness "))
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].origin, Some(MessageOrigin::Generated));
    assert!(notices[0].content.contains(&handle));
    assert!(
        notices[0]
            .content
            .contains("stdout 2000000 bytes, stderr 2000000 bytes")
    );
    assert!(
        provider
            .requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .iter()
            .any(|message| message.starts_with("<harness ") && message.contains(&handle))
    );
    assert_eq!(
        harness
            .prompt(&session.id, "check again", RunOptions::default())
            .await
            .unwrap()
            .status,
        "completed"
    );
    assert_eq!(
        store
            .messages(&session.id)
            .unwrap()
            .iter()
            .filter(|message| message.content.starts_with("<harness "))
            .count(),
        1
    );
    call(&registry, &session.id, "poll", json!({"handle":handle})).await;
    assert_eq!(spool_size(&fixture.data.path().join("jobs")), 0);
    repl.shutdown().await;
    fixture.processes.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn second_host_on_same_data_dir_keeps_live_spool() {
    let fixture = Fixture::new(ExecConfig::default());
    #[cfg(windows)]
    let command = "[Console]::Out.Write('live output'); Start-Sleep -Seconds 30";
    #[cfg(not(windows))]
    let command = "printf 'live output'; sleep 30";
    let started = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command":command,"background":true}),
    )
    .await;
    let jobs = fixture.data.path().join("jobs");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while spool_size(&jobs) == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(spool_size(&jobs), 11);
    let other = ProcessTools::with_config(
        WorkspaceTools::new(fixture._workspace.path()).unwrap(),
        ExecConfig::default(),
        fixture.data.path(),
    )
    .unwrap();
    assert_eq!(spool_size(&jobs), 11, "a live host's spool must survive");
    other.shutdown().await;
    call(
        &fixture.registry,
        "owner",
        "cancel",
        json!({"handle":started["handle"]}),
    )
    .await;
    fixture.processes.shutdown().await;
}

#[cfg(any(unix, windows))]
fn cwd_reset_note(start: &Path) -> String {
    format!(
        "Working directory reset to {}; each exec starts there. Pass `cwd` to run elsewhere.",
        start.display()
    )
}
#[cfg(unix)]
fn quoted(path: &Path) -> String {
    format!("'{}'", path.display())
}

#[cfg(unix)]
#[tokio::test]
async fn cd_inside_command_adds_reset_note_to_the_final_result() {
    let fixture = Fixture::new(ExecConfig::default());
    let elsewhere = tempfile::tempdir().unwrap();
    let other = std::fs::canonicalize(elsewhere.path()).unwrap();
    let start = std::fs::canonicalize(fixture._workspace.path()).unwrap();
    let result = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command": format!("cd {} && pwd", quoted(&other))}),
    )
    .await;
    assert_eq!(result["status"], "exited");
    assert_eq!(result["exitCode"], 0);
    assert_eq!(result["stdout"], format!("{}\n", other.display()));
    assert_eq!(result["note"], cwd_reset_note(&start));
    fixture.processes.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn commands_without_a_directory_change_get_no_note() {
    let fixture = Fixture::new(ExecConfig::default());
    let start = std::fs::canonicalize(fixture._workspace.path()).unwrap();
    let plain = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command": "printf plain"}),
    )
    .await;
    assert_eq!(plain["stdout"], "plain");
    assert!(plain.get("note").is_none());
    // Changing into the start directory is not a change, and a subshell cd
    // does not move the shell that exits.
    let same = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command": format!("cd {} && (cd /; printf same)", quoted(&start))}),
    )
    .await;
    assert_eq!(same["stdout"], "same");
    assert_eq!(same["exitCode"], 0);
    assert!(same.get("note").is_none());
    fixture.processes.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn exit_code_and_output_are_unchanged_after_cd_and_nonzero_exit_gets_note() {
    let fixture = Fixture::new(ExecConfig::default());
    let elsewhere = tempfile::tempdir().unwrap();
    let other = std::fs::canonicalize(elsewhere.path()).unwrap();
    let start = std::fs::canonicalize(fixture._workspace.path()).unwrap();
    let moved = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command": format!("cd {} && printf out && printf err >&2 && exit 7", quoted(&other))}),
    )
    .await;
    assert_eq!(moved["status"], "exited");
    assert_eq!(moved["exitCode"], 7);
    assert_eq!(moved["stdout"], "out");
    assert_eq!(moved["stderr"], "err");
    assert_eq!(moved["note"], cwd_reset_note(&start));
    let unmoved = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command": "printf out && printf err >&2 && exit 7"}),
    )
    .await;
    assert_eq!(unmoved["exitCode"], 7);
    assert_eq!(unmoved["stdout"], "out");
    assert_eq!(unmoved["stderr"], "err");
    assert!(unmoved.get("note").is_none());
    fixture.processes.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn backgrounded_cd_gets_note_only_on_the_final_wait_and_poll() {
    let fixture = Fixture::new(ExecConfig::default());
    let elsewhere = tempfile::tempdir().unwrap();
    let other = std::fs::canonicalize(elsewhere.path()).unwrap();
    let start = std::fs::canonicalize(fixture._workspace.path()).unwrap();
    let started = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({
            "command": format!("cd {}; (sleep 0.3; printf late) & printf now", quoted(&other)),
            "background": true
        }),
    )
    .await;
    assert_eq!(started["status"], "running");
    assert!(started.get("note").is_none());
    let handle = started["handle"].clone();
    let waited = call(
        &fixture.registry,
        "owner",
        "wait",
        json!({"handle": handle, "timeoutMs": 5000}),
    )
    .await;
    assert_eq!(waited["status"], "exited");
    assert_eq!(waited["exitCode"], 0);
    assert_eq!(waited["note"], cwd_reset_note(&start));
    let polled = call(
        &fixture.registry,
        "owner",
        "poll",
        json!({"handle": handle}),
    )
    .await;
    // The backgrounded subshell does not outlive its shell: the tree is killed
    // when the shell exits, so its later output is never captured.
    assert_eq!(polled["stdout"], "now");
    assert_eq!(polled["note"], cwd_reset_note(&start));
    fixture.processes.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn missing_marker_adds_no_note_for_exec_user_trap_or_kill() {
    let fixture = Fixture::new(ExecConfig::default());
    let elsewhere = tempfile::tempdir().unwrap();
    let other = std::fs::canonicalize(elsewhere.path()).unwrap();
    // exec replaces the shell, so its exit trap never runs.
    let replaced = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command": format!("cd {} && exec sh -c 'printf replaced'", quoted(&other))}),
    )
    .await;
    assert_eq!(replaced["stdout"], "replaced");
    assert_eq!(replaced["exitCode"], 0);
    assert!(replaced.get("note").is_none());
    // A command that sets its own EXIT trap replaces the marker trap.
    let trapped = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command": format!("trap 'printf trapped' EXIT; cd {}", quoted(&other))}),
    )
    .await;
    assert_eq!(trapped["stdout"], "trapped");
    assert!(trapped.get("note").is_none());
    // A killed shell never runs its exit trap.
    let killed = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command": format!("cd {}; kill -9 $$", quoted(&other))}),
    )
    .await;
    assert_eq!(killed["status"], "exited");
    assert!(killed["exitCode"].is_null());
    assert!(killed.get("note").is_none());
    fixture.processes.shutdown().await;
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::path::PathBuf;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess,
            WaitForSingleObject,
        },
    };

    fn literal(value: &str) -> String {
        format!("'{}'", value.replace('\'', "''"))
    }

    fn path_literal(path: &Path) -> String {
        literal(&path.to_string_lossy())
    }

    /// Hold the original process object throughout the assertion. PID reuse
    /// cannot make a later liveness check observe an unrelated process.
    struct OwnedProcess {
        handle: HANDLE,
        pid: u32,
    }

    impl OwnedProcess {
        fn open(pid: u32) -> Self {
            // SAFETY: OpenProcess returns an owned handle, closed in Drop.
            let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
            assert!(
                !handle.is_null(),
                "cannot open fixture process {pid}: {}",
                std::io::Error::last_os_error()
            );
            Self { handle, pid }
        }

        fn running(&self) -> bool {
            // SAFETY: the process handle remains valid until Drop.
            let state = unsafe { WaitForSingleObject(self.handle, 0) };
            assert!(
                matches!(state, WAIT_OBJECT_0 | WAIT_TIMEOUT),
                "cannot inspect fixture process {}: {}",
                self.pid,
                std::io::Error::last_os_error()
            );
            state == WAIT_TIMEOUT
        }

        async fn assert_stopped(&self) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while self.running() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!("owned fixture process {} survived job completion", self.pid)
            });
        }

        fn assert_stopped_without_runtime(&self) {
            // SAFETY: the process handle remains valid until Drop. Waiting
            // directly on the kernel object gives no Tokio task a chance to
            // perform cleanup on this test's current-thread executor.
            let state = unsafe { WaitForSingleObject(self.handle, 5000) };
            assert_eq!(
                state, WAIT_OBJECT_0,
                "owned fixture process {} survived owner Drop without executor progress",
                self.pid
            );
        }
    }

    impl Drop for OwnedProcess {
        fn drop(&mut self) {
            // SAFETY: this is the exact owned process object opened above.
            // Cleanup on an assertion failure never targets an image name or
            // reopens a PID that could have been reused.
            unsafe {
                if WaitForSingleObject(self.handle, 0) == WAIT_TIMEOUT {
                    TerminateProcess(self.handle, 1);
                    WaitForSingleObject(self.handle, 2000);
                }
                CloseHandle(self.handle);
            }
        }
    }

    struct TreeScripts {
        command: String,
        root_pid: PathBuf,
        child_pid: PathBuf,
        grandchild_pid: PathBuf,
        ready: PathBuf,
        release: PathBuf,
    }

    impl TreeScripts {
        fn new(root: &Path, natural_exit: bool) -> Self {
            let root_pid = root.join("root.pid");
            let child_pid = root.join("child.pid");
            let grandchild_pid = root.join("grandchild.pid");
            let ready = root.join("grandchild.ready");
            let release = root.join("release-child");
            let grandchild_script = root.join("grandchild.ps1");
            let child_script = root.join("child.ps1");
            std::fs::write(
                &grandchild_script,
                format!(
                    "[IO.File]::WriteAllText({}, 'ready'); Start-Sleep -Seconds 60",
                    path_literal(&ready)
                ),
            )
            .unwrap();
            let child_end = if natural_exit {
                format!(
                    "while (-not [IO.File]::Exists({})) {{ Start-Sleep -Milliseconds 25 }}; exit 0",
                    path_literal(&release)
                )
            } else {
                "Start-Sleep -Seconds 60".to_owned()
            };
            std::fs::write(
                &child_script,
                format!(
                    concat!(
                        "[IO.File]::WriteAllText({}, [string]$PID); ",
                        "$start = New-Object System.Diagnostics.ProcessStartInfo; ",
                        "$start.FileName = Join-Path $PSHOME 'powershell.exe'; ",
                        "$start.Arguments = {}; ",
                        "$start.UseShellExecute = $false; $start.CreateNoWindow = $true; ",
                        "$grandchild = [Diagnostics.Process]::Start($start); ",
                        "[IO.File]::WriteAllText({}, [string]$grandchild.Id); {}"
                    ),
                    path_literal(&child_pid),
                    literal(&format!(
                        "-NoProfile -NonInteractive -File \"{}\"",
                        grandchild_script.display()
                    )),
                    path_literal(&grandchild_pid),
                    child_end,
                ),
            )
            .unwrap();
            let command = format!(
                "[IO.File]::WriteAllText({}, [string]$PID); & (Join-Path $PSHOME 'powershell.exe') -NoProfile -NonInteractive -File {}; exit $LASTEXITCODE",
                path_literal(&root_pid),
                path_literal(&child_script)
            );
            Self {
                command,
                root_pid,
                child_pid,
                grandchild_pid,
                ready,
                release,
            }
        }

        async fn observe(&self) -> [OwnedProcess; 3] {
            async fn process(path: &Path) -> OwnedProcess {
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if let Ok(text) = std::fs::read_to_string(path)
                            && let Ok(pid) = text.trim().parse()
                        {
                            return OwnedProcess::open(pid);
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("fixture did not publish {}", path.display()))
            }
            let root = process(&self.root_pid).await;
            let child = process(&self.child_pid).await;
            let grandchild = process(&self.grandchild_pid).await;
            tokio::time::timeout(Duration::from_secs(10), async {
                while !self.ready.exists() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("grandchild did not reach its ready marker");
            assert!(root.running(), "root exited before the test action");
            assert!(child.running(), "child exited before the test action");
            assert!(
                grandchild.running(),
                "grandchild exited before the test action"
            );
            [root, child, grandchild]
        }
    }

    #[tokio::test]
    async fn cancel_terminates_the_shell_child_and_grandchild() {
        let fixture = Fixture::new(ExecConfig::default());
        let scripts = TreeScripts::new(fixture._workspace.path(), false);
        let started = call(
            &fixture.registry,
            "owner",
            "exec",
            json!({"command":scripts.command,"background":true}),
        )
        .await;
        assert_eq!(started["status"], "running");
        let owned = scripts.observe().await;
        call(
            &fixture.registry,
            "owner",
            "cancel",
            json!({"handle":started["handle"]}),
        )
        .await;
        let done = call(
            &fixture.registry,
            "owner",
            "wait",
            json!({"handle":started["handle"],"timeoutMs":10000}),
        )
        .await;
        assert_eq!(done["status"], "cancelled");
        for process in &owned {
            process.assert_stopped().await;
        }
        fixture.processes.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_terminates_the_shell_child_and_grandchild() {
        let fixture = Fixture::new(ExecConfig::default());
        let scripts = TreeScripts::new(fixture._workspace.path(), false);
        let started = call(
            &fixture.registry,
            "owner",
            "exec",
            json!({"command":scripts.command,"background":true}),
        )
        .await;
        assert_eq!(started["status"], "running");
        let owned = scripts.observe().await;
        fixture.processes.shutdown().await;
        for process in &owned {
            process.assert_stopped().await;
        }
        assert!(
            fixture.processes.jobs("owner").unwrap()["jobs"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_last_host_owner_terminates_the_tree_without_executor_progress() {
        let fixture = Fixture::new(ExecConfig::default());
        let scripts = TreeScripts::new(fixture._workspace.path(), false);
        let started = call(
            &fixture.registry,
            "owner",
            "exec",
            json!({"command":scripts.command,"background":true}),
        )
        .await;
        assert_eq!(started["status"], "running");
        let owned = scripts.observe().await;
        let Fixture {
            _workspace: workspace,
            data,
            processes,
            registry,
        } = fixture;
        // Registered tools hold ProcessTools Arcs too. Release those first,
        // while the fixture directories remain alive until after assertions.
        drop(registry);
        assert_eq!(Arc::strong_count(&processes), 1);
        drop(processes);
        for process in &owned {
            process.assert_stopped_without_runtime();
        }
        drop(data);
        drop(workspace);
    }

    #[tokio::test]
    async fn hard_deadline_terminates_the_shell_child_and_grandchild() {
        let fixture = Fixture::new(ExecConfig::default());
        let scripts = TreeScripts::new(fixture._workspace.path(), false);
        let started = call(
            &fixture.registry,
            "owner",
            "exec",
            json!({"command":scripts.command,"background":true,"timeoutMs":15000}),
        )
        .await;
        let owned = scripts.observe().await;
        let done = call(
            &fixture.registry,
            "owner",
            "wait",
            json!({"handle":started["handle"],"timeoutMs":20000}),
        )
        .await;
        assert_eq!(done["status"], "timed_out");
        for process in &owned {
            process.assert_stopped().await;
        }
        fixture.processes.shutdown().await;
    }

    #[tokio::test]
    async fn natural_shell_exit_releases_remaining_owned_grandchild() {
        let fixture = Fixture::new(ExecConfig::default());
        let scripts = TreeScripts::new(fixture._workspace.path(), true);
        let started = call(
            &fixture.registry,
            "owner",
            "exec",
            json!({"command":scripts.command,"background":true}),
        )
        .await;
        let owned = scripts.observe().await;
        std::fs::write(&scripts.release, "exit").unwrap();
        let done = call(
            &fixture.registry,
            "owner",
            "wait",
            json!({"handle":started["handle"],"timeoutMs":10000}),
        )
        .await;
        assert_eq!(done["status"], "exited");
        assert_eq!(done["exitCode"], 0);
        for process in &owned {
            process.assert_stopped().await;
        }
        fixture.processes.shutdown().await;
    }

    #[tokio::test]
    async fn default_shell_remains_windows_powershell() {
        let fixture = Fixture::new(ExecConfig::default());
        let result = call(
            &fixture.registry,
            "owner",
            "exec",
            json!({"command":"[Console]::Out.Write([Diagnostics.Process]::GetCurrentProcess().MainModule.ModuleName + '|' + $PSVersionTable.PSEdition)"}),
        )
        .await;
        assert_eq!(result["status"], "exited");
        assert_eq!(result["exitCode"], 0);
        assert_eq!(result["stdout"], "powershell.exe|Desktop");
        assert_eq!(result["stderr"], "");
        assert!(result.get("note").is_none());
        fixture.processes.shutdown().await;
    }

    #[tokio::test]
    async fn successful_cd_reports_reset_to_the_explicit_start_directory() {
        let fixture = Fixture::new(ExecConfig::default());
        let custom = fixture._workspace.path().join("custom start");
        std::fs::create_dir(&custom).unwrap();
        let start = std::fs::canonicalize(&custom).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let result = call(
            &fixture.registry,
            "owner",
            "exec",
            json!({
                "cwd":"custom start",
                "command":format!("Set-Location -LiteralPath {}; [Console]::Out.Write((Get-Location).ProviderPath)", path_literal(elsewhere.path()))
            }),
        )
        .await;
        assert_eq!(result["status"], "exited");
        assert_eq!(result["exitCode"], 0);
        assert_eq!(
            std::fs::canonicalize(result["stdout"].as_str().unwrap()).unwrap(),
            std::fs::canonicalize(elsewhere.path()).unwrap()
        );
        assert_eq!(result["stderr"], "");
        assert_eq!(result["note"], cwd_reset_note(&start));
        fixture.processes.shutdown().await;
    }

    #[tokio::test]
    async fn changed_directory_adds_note_and_preserves_explicit_exit_and_exact_streams() {
        let fixture = Fixture::new(ExecConfig::default());
        let elsewhere = tempfile::tempdir().unwrap();
        let start = std::fs::canonicalize(fixture._workspace.path()).unwrap();
        let output = "[Console]::Out.Write('out'); [Console]::Error.Write('err'); exit 3";
        for moved in [false, true] {
            let command = if moved {
                format!(
                    "Set-Location -LiteralPath {}; {output}",
                    path_literal(elsewhere.path())
                )
            } else {
                output.to_owned()
            };
            let result = call(
                &fixture.registry,
                "owner",
                "exec",
                json!({"command":command}),
            )
            .await;
            assert_eq!(result["status"], "exited");
            assert_eq!(result["exitCode"], 3);
            assert_eq!(result["stdout"], "out");
            assert_eq!(result["stderr"], "err");
            if moved {
                assert_eq!(result["note"], cwd_reset_note(&start));
            } else {
                assert!(result.get("note").is_none());
            }
        }
        fixture.processes.shutdown().await;
    }

    #[tokio::test]
    async fn unchanged_and_restored_directories_get_no_note() {
        let fixture = Fixture::new(ExecConfig::default());
        let elsewhere = tempfile::tempdir().unwrap();
        let start = std::fs::canonicalize(fixture._workspace.path()).unwrap();
        for command in [
            "[Console]::Out.Write('same')".to_owned(),
            format!(
                "Set-Location -LiteralPath {}; [Console]::Out.Write('same')",
                path_literal(fixture._workspace.path())
            ),
            format!(
                "Set-Location -LiteralPath {}; Set-Location -LiteralPath {}; [Console]::Out.Write('same')",
                path_literal(elsewhere.path()),
                path_literal(fixture._workspace.path())
            ),
        ] {
            let result = call(
                &fixture.registry,
                "owner",
                "exec",
                json!({"command":command}),
            )
            .await;
            assert_eq!(result["exitCode"], 0);
            assert_eq!(result["stdout"], "same");
            assert!(result.get("note").is_none(), "{result}");
        }
        // Every invocation starts back in the configured directory.
        let location = call(
            &fixture.registry,
            "owner",
            "exec",
            json!({"command":"[Console]::Out.Write((Get-Location).ProviderPath)"}),
        )
        .await;
        assert_eq!(
            std::fs::canonicalize(location["stdout"].as_str().unwrap()).unwrap(),
            start
        );
        fixture.processes.shutdown().await;
    }

    #[tokio::test]
    async fn cwd_note_preserves_powershell_native_failure_and_native_template_exit_code() {
        let fixture = Fixture::new(ExecConfig::default());
        let elsewhere = tempfile::tempdir().unwrap();
        let start = std::fs::canonicalize(fixture._workspace.path()).unwrap();
        let native =
            "[Console]::Out.Write('out'); [Console]::Error.Write('err'); & cmd.exe /d /c 'exit 7'";
        for moved in [false, true] {
            let command = if moved {
                format!(
                    "Set-Location -LiteralPath {}; {native}",
                    path_literal(elsewhere.path())
                )
            } else {
                native.to_owned()
            };
            let result = call(
                &fixture.registry,
                "owner",
                "exec",
                json!({"command":command}),
            )
            .await;
            // PowerShell -Command maps a final failing native command to 1.
            // Only explicit `exit $LASTEXITCODE` exposes the native exit code.
            assert_eq!(result["exitCode"], 1);
            assert_eq!(result["stdout"], "out");
            assert_eq!(result["stderr"], "err");
            if moved {
                assert_eq!(result["note"], cwd_reset_note(&start));
            } else {
                assert!(result.get("note").is_none());
            }
        }
        let native_template = call(
            &fixture.registry,
            "owner",
            "exec",
            json!({"command":{"parts":["cmd.exe /d /c ",""],"values":["exit 7"]}}),
        )
        .await;
        assert_eq!(native_template["status"], "exited");
        assert_eq!(native_template["exitCode"], 7);
        assert!(native_template.get("note").is_none());
        fixture.processes.shutdown().await;
    }

    #[tokio::test]
    async fn changed_directory_keeps_terminating_and_nonterminating_cmdlet_errors() {
        let fixture = Fixture::new(ExecConfig::default());
        let elsewhere = tempfile::tempdir().unwrap();
        let start = std::fs::canonicalize(fixture._workspace.path()).unwrap();
        for error in [
            "Write-Error 'bp-winexec-preserved-error'",
            "Write-Error 'bp-winexec-preserved-error' -ErrorAction Stop",
        ] {
            let result = call(
                &fixture.registry,
                "owner",
                "exec",
                json!({"command":format!("Set-Location -LiteralPath {}; [Console]::Out.Write('before'); {error}", path_literal(elsewhere.path()))}),
            )
            .await;
            assert_eq!(result["status"], "exited");
            assert_eq!(result["exitCode"], 1);
            assert_eq!(result["stdout"], "before");
            assert!(
                result["stderr"]
                    .as_str()
                    .unwrap()
                    .contains("bp-winexec-preserved-error"),
                "{result}"
            );
            assert_eq!(result["note"], cwd_reset_note(&start));
        }
        fixture.processes.shutdown().await;
    }

    #[tokio::test]
    async fn background_directory_note_appears_on_final_wait_and_poll() {
        let fixture = Fixture::new(ExecConfig::default());
        let elsewhere = tempfile::tempdir().unwrap();
        let start = std::fs::canonicalize(fixture._workspace.path()).unwrap();
        let started = call(
            &fixture.registry,
            "owner",
            "exec",
            json!({
                "command":format!("Set-Location -LiteralPath {}; [Console]::Out.Write('now'); Start-Sleep -Milliseconds 300; [Console]::Out.Write('late'); [Console]::Error.Write('err'); exit 3", path_literal(elsewhere.path())),
                "background":true
            }),
        )
        .await;
        assert_eq!(started["status"], "running");
        assert!(started.get("note").is_none());
        let waited = call(
            &fixture.registry,
            "owner",
            "wait",
            json!({"handle":started["handle"],"timeoutMs":10000}),
        )
        .await;
        assert_eq!(waited["status"], "exited");
        assert_eq!(waited["exitCode"], 3);
        assert_eq!(waited["note"], cwd_reset_note(&start));
        let polled = call(
            &fixture.registry,
            "owner",
            "poll",
            json!({"handle":started["handle"]}),
        )
        .await;
        assert_eq!(polled["exitCode"], 3);
        assert_eq!(polled["stdout"], "nowlate");
        assert_eq!(polled["stderr"], "err");
        assert_eq!(polled["note"], cwd_reset_note(&start));
        fixture.processes.shutdown().await;
    }
}

#[tokio::test]
async fn live_exec_config_changes_next_admission_but_preserves_foreground_wait() {
    let fixture = Fixture::new(ExecConfig::default());
    let registry = fixture.registry.clone();
    let running =
        tokio::spawn(
            async move { call(&registry, "owner", "exec", json!({"command":DELAYED})).await },
        );
    foreground(&fixture.processes, "owner").await;
    fixture.processes.apply_config(ExecConfig {
        sync_timeout_secs: 0,
        max_jobs: Some(1),
        max_jobs_per_session: Some(1),
        ..ExecConfig::default()
    });
    let error = fixture
        .registry
        .get("exec")
        .unwrap()
        .invoke(context("other"), json!({"command":ECHO}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("max_jobs limit"));
    let done = running.await.unwrap();
    assert_eq!(done["status"], "exited");
    assert_eq!(done["stdout"], "firstlast");
    let next = call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command":DELAYED}),
    )
    .await;
    assert_eq!(next["status"], "backgrounded");
    assert_eq!(next["reason"], "sync_timeout");
    fixture.processes.apply_config(ExecConfig::default());
    let other = call(&fixture.registry, "other", "exec", json!({"command":ECHO})).await;
    assert_eq!(other["status"], "exited");
    fixture.processes.shutdown().await;
}

#[path = "exec_jobs/output_cap_reload.rs"]
mod output_cap_reload;
