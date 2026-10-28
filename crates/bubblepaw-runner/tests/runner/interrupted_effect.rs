use super::*;

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
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let command = windows_command::powershell_file(
        &workspace,
        "interrupted-effect.ps1",
        "[IO.File]::WriteAllText('effect-child.pid', [string]$PID); Add-Content counter.txt effect; Start-Sleep -Seconds 30; Set-Content effect-finished.txt done",
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let app = axum::Router::new().route("/responses", axum::routing::post(|axum::extract::State(command): axum::extract::State<Value>, axum::Json(request): axum::Json<Value>| async move {
            assert_eq!(request["tools"].as_array().unwrap().len(), 1);
            assert_eq!(request["tools"][0]["name"], "js");
            let arguments = json!({"code":format!("await tools.exec({});", json!({"command":command,"yieldMs":30000,"timeoutMs":60000}))});
            let complete = json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call","id":"item-fixture","call_id":"crash-effect","name":"js","arguments":arguments.to_string()}]}});
            ([ ("content-type", "text/event-stream") ], format!("data: {complete}\n\n"))
        })).with_state(command);
        axum::serve(listener, app).await.unwrap();
    });
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
