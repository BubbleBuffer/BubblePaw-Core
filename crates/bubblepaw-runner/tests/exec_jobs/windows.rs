use super::*;
use std::path::PathBuf;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
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
        tokio::time::timeout(Duration::from_secs(30), async {
            while self.running() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("owned fixture process {} survived job completion", self.pid));
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
    command: Value,
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
                    "-NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\"",
                    grandchild_script.display()
                )),
                path_literal(&grandchild_pid),
                child_end,
            ),
        )
        .unwrap();
        let command = windows_command::powershell_file(
            root,
            "root.ps1",
            &format!(
                "[IO.File]::WriteAllText({}, [string]$PID); & (Join-Path $PSHOME 'powershell.exe') -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {}; exit $LASTEXITCODE",
                path_literal(&root_pid),
                path_literal(&child_script)
            ),
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
            tokio::time::timeout(Duration::from_secs(40), async {
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
        tokio::time::timeout(Duration::from_secs(40), async {
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
        json!({"handle":started["handle"],"timeoutMs":60000}),
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
        json!({"command":scripts.command,"background":true,"timeoutMs":45000}),
    )
    .await;
    let owned = scripts.observe().await;
    let done = call(
        &fixture.registry,
        "owner",
        "wait",
        json!({"handle":started["handle"],"timeoutMs":90000}),
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
        json!({"handle":started["handle"],"timeoutMs":60000}),
    )
    .await;
    assert_eq!(done["status"], "exited");
    assert_eq!(done["exitCode"], 0);
    for process in &owned {
        process.assert_stopped().await;
    }
    fixture.processes.shutdown().await;
}

#[path = "windows_cmd.rs"]
mod cmd_tests;
