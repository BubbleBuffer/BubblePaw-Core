use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
};

fn cli(root: &Path, db: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
    command
        .args(["--provider", "demo", "--json", "--workspace"])
        .arg(root)
        .arg("--db")
        .arg(db);
    command
}

#[path = "runner/storage.rs"]
mod storage;

async fn send(stdin: &mut ChildStdin, id: i32, method: &str, params: Value) {
    let mut bytes =
        serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .unwrap();
    bytes.push(b'\n');
    stdin.write_all(&bytes).await.unwrap();
    stdin.flush().await.unwrap();
}
async fn next(stdout: &mut BufReader<ChildStdout>) -> Value {
    next_for(stdout, "runner event").await
}
async fn next_for(stdout: &mut BufReader<ChildStdout>, phase: &str) -> Value {
    let mut line = String::new();
    let count = tokio::time::timeout(Duration::from_secs(10), stdout.read_line(&mut line))
        .await
        .unwrap_or_else(|error| panic!("runner timed out waiting for {phase}: {error}"))
        .unwrap_or_else(|error| panic!("runner stdout failed while waiting for {phase}: {error}"));
    assert!(count > 0, "runner closed stdout while waiting for {phase}");
    serde_json::from_str(&line).unwrap()
}
async fn response(stdout: &mut BufReader<ChildStdout>, id: i32) -> Value {
    loop {
        let record = next_for(stdout, &format!("RPC response id={id}")).await;
        if record["id"] == id {
            assert!(record.get("error").is_none(), "{record}");
            return record["result"].clone();
        }
    }
}

#[tokio::test]
async fn stdio_accepts_work_and_exits_with_stdin_still_open() {
    let temp = tempfile::tempdir().unwrap();
    let mut child =
        tokio::process::Command::from(cli(temp.path(), &temp.path().join("archive.sqlite")))
            .args(["--read-only", "stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    send(
        &mut stdin,
        1,
        "session.open",
        json!({"title":"stdio acceptance"}),
    )
    .await;
    let session = response(&mut stdout, 1).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    send(
        &mut stdin,
        2,
        "run.start",
        json!({"session_id":session,"text":"Run the demo"}),
    )
    .await;
    let admitted = response(&mut stdout, 2).await;
    assert_eq!(admitted["status"], "running");
    loop {
        let event = next(&mut stdout).await;
        if event["params"]["type"] == "session_settled" {
            break;
        }
    }
    send(
        &mut stdin,
        3,
        "session.state",
        json!({"session_id":session}),
    )
    .await;
    let state = response(&mut stdout, 3).await;
    assert!(
        state["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "assistant")
    );
    send(&mut stdin, 4, "shutdown", json!({})).await;
    assert_eq!(response(&mut stdout, 4).await["stopped"], true);
    // Deliberately retain stdin: shutdown must not wait for an EOF from the host.
    assert!(
        tokio::time::timeout(Duration::from_secs(4), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    drop(stdin);
}

#[tokio::test]
async fn stdio_can_cancel_an_outstanding_provider_request() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (entered_tx, mut entered) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/responses",
            axum::routing::post(move || {
                let entered_tx = entered_tx.clone();
                async move {
                    let _ = entered_tx.send(());
                    std::future::pending::<()>().await;
                    "never completed"
                }
            }),
        );
        axum::serve(listener, app).await.unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_bubblepaw"))
        .args([
            "--provider",
            "api",
            "--base-url",
            &format!("http://{address}"),
            "--decider",
            "llm/api/gpt-6-luna",
            "--workspace",
        ])
        .arg(temp.path())
        .arg("--db")
        .arg(temp.path().join("archive.sqlite"))
        .arg("stdio")
        .env("OPENAI_API_KEY", "synthetic-fixture")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    send(&mut stdin, 1, "session.open", json!({})).await;
    let session = response(&mut stdout, 1).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    send(
        &mut stdin,
        2,
        "run.start",
        json!({"session_id":session,"text":"Wait for provider"}),
    )
    .await;
    response(&mut stdout, 2).await;
    tokio::time::timeout(Duration::from_secs(10), entered.recv())
        .await
        .expect("local /responses handler was not entered before cancellation")
        .expect("local provider fixture stopped before receiving the request");
    send(&mut stdin, 3, "run.cancel", json!({"session_id":session})).await;
    assert_eq!(response(&mut stdout, 3).await["requested"], true);
    send(&mut stdin, 4, "shutdown", json!({})).await;
    response(&mut stdout, 4).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(4), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    server.abort();
}

#[cfg(unix)]
async fn stdio_wakes_for_completion(code: &str, notice_prefix: &str) {
    let temp = tempfile::tempdir().unwrap();
    let mut child =
        tokio::process::Command::from(cli(temp.path(), &temp.path().join("archive.sqlite")))
            .args([
                "--no-auto-summarize",
                "--set",
                "agents.wake_debounce_ms=100",
                "--set",
                "agent.execution_mode='js'",
                "stdio",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    send(&mut stdin, 1, "session.open", json!({})).await;
    let session = response(&mut stdout, 1).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    send(
        &mut stdin,
        2,
        "run.start",
        json!({"session_id":session,"text":"Start background work","script":code}),
    )
    .await;
    let original = response(&mut stdout, 2).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut wake = None;
    let mut streamed = false;
    loop {
        let event = next(&mut stdout).await;
        let event = &event["params"];
        if event["session_id"] != session || event["run_id"] == original {
            continue;
        }
        if event["type"] == "run_started" {
            assert!(wake.is_none(), "more than one automatic wake");
            wake = event["run_id"].as_str().map(str::to_owned);
        }
        if event["type"] == "paragraph" {
            streamed = true;
        }
        if event["type"] == "session_settled" {
            break;
        }
    }
    assert!(wake.is_some(), "no auto-start notification");
    assert!(streamed, "wake did not stream through stdio events");
    send(
        &mut stdin,
        3,
        "session.state",
        json!({"session_id":session}),
    )
    .await;
    let state = response(&mut stdout, 3).await;
    let notices = state["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| {
            message["origin"] == "generated"
                && message["content"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("<harness "))
        })
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 1, "{state}");
    assert!(
        notices[0]["content"]
            .as_str()
            .unwrap()
            .contains(notice_prefix)
    );
    assert_eq!(state["runs"].as_array().unwrap().len(), 2);
    send(&mut stdin, 4, "shutdown", json!({})).await;
    response(&mut stdout, 4).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(4), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_wakes_an_idle_parent_when_a_background_exec_finishes() {
    stdio_wakes_for_completion(
        "return await tools.exec({command:'sleep 0.2; printf wake',background:true});",
        "Job process:",
    )
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_wakes_an_idle_parent_when_a_visible_child_finishes() {
    stdio_wakes_for_completion(
        "return await tools.agent({profile:'workspace',task:'Complete the offline demo'});",
        "agent:",
    )
    .await;
}

#[cfg(windows)]
struct OwnedEffectProcess {
    handle: windows_sys::Win32::Foundation::HANDLE,
    pid: u32,
}

#[cfg(windows)]
impl OwnedEffectProcess {
    fn open(pid: u32) -> Self {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        };
        // SAFETY: this owned handle is kept through the host-kill assertion
        // and closed by Drop, so later checks cannot observe a reused PID.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
        assert!(
            !handle.is_null(),
            "cannot open fixture effect process {pid}: {}",
            std::io::Error::last_os_error()
        );
        Self { handle, pid }
    }

    fn assert_running(&self) {
        use windows_sys::Win32::{
            Foundation::WAIT_TIMEOUT, System::Threading::WaitForSingleObject,
        };
        // SAFETY: the held process handle remains valid until Drop.
        assert_eq!(
            unsafe { WaitForSingleObject(self.handle, 0) },
            WAIT_TIMEOUT,
            "fixture effect process {} exited before the host kill",
            self.pid
        );
    }

    fn assert_exited_after_host_kill(&self) {
        use windows_sys::Win32::{
            Foundation::WAIT_OBJECT_0, System::Threading::WaitForSingleObject,
        };
        // SAFETY: this is the original held process object. Five seconds is
        // shorter than the fixture's 30-second delay before its final effect.
        assert_eq!(
            unsafe { WaitForSingleObject(self.handle, 5000) },
            WAIT_OBJECT_0,
            "fixture effect process {} survived the abrupt host kill",
            self.pid
        );
    }
}

#[cfg(windows)]
impl Drop for OwnedEffectProcess {
    fn drop(&mut self) {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, WAIT_TIMEOUT},
            System::Threading::{TerminateProcess, WaitForSingleObject},
        };
        // SAFETY: cleanup targets only the exact fixture process handle. It
        // never reopens a PID or targets an executable name after failure.
        unsafe {
            if WaitForSingleObject(self.handle, 0) == WAIT_TIMEOUT {
                TerminateProcess(self.handle, 1);
                WaitForSingleObject(self.handle, 2000);
            }
            CloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
#[tokio::test]
async fn killed_runner_recovers_unknown_effect_without_repeating_it() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let app = axum::Router::new().route("/responses", axum::routing::post(|axum::Json(request): axum::Json<Value>| async move {
            assert_eq!(request["tools"].as_array().unwrap().len(), 1);
            assert_eq!(request["tools"][0]["name"], "js");
            let command = "[IO.File]::WriteAllText('effect-child.pid', [string]$PID); Add-Content counter.txt effect; Start-Sleep -Seconds 30; Set-Content effect-finished.txt done";
            let arguments = json!({"code":format!("await tools.exec({});", json!({"command":command,"yieldMs":30000,"timeoutMs":60000}))});
            let complete = json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call","id":"item-fixture","call_id":"crash-effect","name":"js","arguments":arguments.to_string()}]}});
            ([ ("content-type", "text/event-stream") ], format!("data: {complete}\n\n"))
        }));
        axum::serve(listener, app).await.unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let db = temp.path().join("archive.sqlite");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_bubblepaw"))
        .args(["--set", "agent.execution_mode='js'"])
        .args([
            "--provider",
            "api",
            "--base-url",
            &format!("http://{address}"),
            "--decider",
            "llm/api/gpt-6-luna",
            "--workspace",
        ])
        .arg(&workspace)
        .arg("--db")
        .arg(&db)
        .arg("stdio")
        .env("OPENAI_API_KEY", "synthetic-fixture")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    send(&mut stdin, 1, "session.open", json!({})).await;
    let session = response(&mut stdout, 1).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    send(
        &mut stdin,
        2,
        "run.start",
        json!({"session_id":session,"text":"Exercise interrupted effect"}),
    )
    .await;
    response(&mut stdout, 2).await;
    let counter = workspace.join("counter.txt");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !std::fs::read_to_string(&counter).is_ok_and(|text| text.trim() == "effect") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    // Publishing the PID precedes the completed counter write, so the marker
    // is fully written before we open and hold the original process object.
    let effect_pid = std::fs::read_to_string(workspace.join("effect-child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let effect_process = OwnedEffectProcess::open(effect_pid);
    effect_process.assert_running();
    // Use the owned process handle: restricted Windows users may be unable to
    // reopen the same child by PID through taskkill. This is still an abrupt
    // termination, while the dispatched effect is demonstrably in flight.
    let finished = workspace.join("effect-finished.txt");
    assert!(!finished.exists());
    child.kill().await.unwrap();
    assert!(!child.wait().await.unwrap().success());
    effect_process.assert_exited_after_host_kill();
    assert!(
        !finished.exists(),
        "the killed effect reached its final marker"
    );
    let history = cli(&workspace, &db)
        .args(["history", &session])
        .output()
        .unwrap();
    assert!(
        history.status.success(),
        "{}",
        String::from_utf8_lossy(&history.stderr)
    );
    let messages: Vec<Value> = serde_json::from_slice(&history.stdout).unwrap();
    assert!(
        messages.iter().any(|m| m["role"] == "tool"
            && m["is_error"] == true
            && m["content"].as_str().unwrap().contains("unknown")),
        "{messages:?}"
    );
    let resumed = cli(&workspace, &db)
        .args(["--read-only", "resume", &session])
        .output()
        .unwrap();
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(std::fs::read_to_string(counter).unwrap().lines().count(), 1);
    assert!(
        !finished.exists(),
        "recovery must not replay the killed effect"
    );
    server.abort();
}

/// Real Linux runner/extension transport and a bounded synthetic shell effect.
/// Killing only the provider child would leave the host tool running, so kill
/// the owned runner while its extension-origin js -> exec effect is in flight.
#[cfg(unix)]
#[tokio::test]
async fn killed_extension_runner_recovers_unknown_effect_and_blocks_reused_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let data = temp.path().join("data");
    let extensions = temp.path().join("extensions.json");
    let step_log = temp.path().join("steps.jsonl");
    let extension_exit = temp.path().join("extension-exited.txt");
    std::fs::write(
        &extensions,
        serde_json::to_vec(&json!([{
            "id":"fake", "command":env!("CARGO_BIN_EXE_bubblepaw-fake-extension"),
            "args":[], "cwd":workspace
        }]))
        .unwrap(),
    )
    .unwrap();
    let effect_code = "await tools.exec({command:'echo effect >> counter.txt; sleep 5; printf done > effect-finished.txt',yieldMs:30000,timeoutMs:10000});";
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
        command.args(["--set", "agent.execution_mode='js'"]);
        command
            .args([
                "--provider",
                "demo",
                "--model",
                "fake/code-model",
                "--json",
                "--approve-all",
                "--workspace",
            ])
            .arg(&workspace)
            .arg("--data-dir")
            .arg(&data)
            .arg("--extensions")
            .arg(&extensions)
            .env("BP_FAKE_EFFECT_CODE", effect_code)
            .env("BP_FAKE_STEP_LOG", &step_log)
            .env("BP_FAKE_EXIT_MARKER", &extension_exit);
        command
    };
    let mut child = tokio::process::Command::from(command())
        .arg("stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    send(
        &mut stdin,
        1,
        "session.open",
        json!({"title":"synthetic extension interruption"}),
    )
    .await;
    let session = response(&mut stdout, 1).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    send(
        &mut stdin,
        2,
        "run.start",
        json!({"session_id":session,"text":"Exercise extension effect"}),
    )
    .await;
    let admitted = response(&mut stdout, 2).await;
    let run_id = admitted["id"].as_str().unwrap().to_owned();
    let counter = workspace.join("counter.txt");
    let finished = workspace.join("effect-finished.txt");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !counter.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !finished.exists(),
        "effect must still be in flight at abrupt host termination"
    );
    child.kill().await.unwrap();
    assert!(!child.wait().await.unwrap().success());
    drop(stdin);
    // The shell's only remaining effect is a completion marker. Await that and
    // the extension's stdin-EOF exit marker; leave no fixture descendants.
    tokio::time::timeout(Duration::from_secs(10), async {
        while !finished.exists() || !extension_exit.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let store = bubblepaw_core::Store::open(&data).unwrap();
    assert_eq!(store.run(&run_id).unwrap().status, "interrupted");
    let recovered = store.messages(&session).unwrap();
    assert!(
        recovered
            .iter()
            .any(|message| message.role == bubblepaw_core::Role::Tool
                && message.tool_call_id.as_deref() == Some("interrupted-extension-effect")
                && message.is_error
                && message.content.contains("outcome unknown")),
        "{recovered:?}"
    );
    // Outer dispatch_tx stores call_id, with its full call owned by the
    // assistant message; pending_effects only hydrates nested effect detail.
    // Check the actual authoritative outer boundary rather than null detail.
    let assert_outer_unknown = |store: &bubblepaw_core::Store| {
        let connection =
            rusqlite::Connection::open(store.chat_path(&session).unwrap().unwrap()).unwrap();
        let count: i64 = connection.query_row(
            "SELECT count(*) FROM effects WHERE session_id=?1 AND run_id=?2 AND call_id=?3 AND state='unknown'",
            rusqlite::params![session, run_id, "interrupted-extension-effect"],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(
            count, 1,
            "outer extension effect must remain durably unknown exactly once"
        );
        let all_dispatches: i64 = connection
            .query_row(
                "SELECT count(*) FROM effects WHERE session_id=?1 AND call_id=?2",
                rusqlite::params![session, "interrupted-extension-effect"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            all_dispatches, 1,
            "reused outer effect crossed dispatch boundary"
        );
    };
    assert_outer_unknown(&store);
    let pending = store.pending_effects(&session).unwrap();
    assert!(
        pending.iter().any(|effect| effect["state"] == "unknown"
            && effect["run_id"] == run_id
            && effect["call"]["name"] == "exec"
            && effect["call"]["arguments"]["command"]
                == "echo effect >> counter.txt; sleep 5; printf done > effect-finished.txt"),
        "{pending:?}"
    );
    drop(store);
    let before: Vec<Value> = std::fs::read_to_string(&step_log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(before.len(), 1);
    // Restarted fake deliberately reissues the same durable ID. A generation
    // request is allowed; the effect must fail the host guard before dispatch.
    let resumed = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::from(command())
            .args(["resume", &session])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    let records: Vec<Value> = String::from_utf8(resumed.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let result = records
        .iter()
        .find(|record| record["type"] == "result")
        .unwrap();
    assert_eq!(result["run"]["status"], "failed", "{result}");
    assert!(
        result["run"]["error"].as_str().unwrap().contains("reused"),
        "{result}"
    );
    assert_eq!(
        std::fs::read_to_string(&counter)
            .unwrap()
            .matches("effect")
            .count(),
        1
    );
    let after: Vec<Value> = std::fs::read_to_string(&step_log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(after.len(), 2, "unexpected generation retries");
    assert_ne!(after[0]["pid"], after[1]["pid"]);
    assert!(
        after[1]["params"]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool"
                && message["tool_call_id"] == "interrupted-extension-effect"
                && message["content"]
                    .as_str()
                    .unwrap()
                    .contains("outcome unknown"))
    );
    assert!(
        !records
            .iter()
            .any(|record| record["type"] == "tool_started"),
        "reused effect reached dispatch"
    );
    let store = bubblepaw_core::Store::open(&data).unwrap();
    assert_outer_unknown(&store);
    assert_eq!(store.messages(&session).unwrap(), recovered);
}

async fn try_next(stdout: &mut BufReader<ChildStdout>, wait: Duration) -> Option<Value> {
    let mut line = String::new();
    match tokio::time::timeout(wait, stdout.read_line(&mut line)).await {
        Ok(Ok(count)) if count > 0 => Some(serde_json::from_str(&line).unwrap()),
        _ => None,
    }
}

#[path = "runner/event_gap_recovery.rs"]
mod event_gap_recovery;
