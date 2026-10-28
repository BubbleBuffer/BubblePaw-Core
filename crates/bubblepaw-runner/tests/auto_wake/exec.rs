//! Background exec helpers shared by the auto-wake tests.
use super::*;

#[cfg(windows)]
#[path = "../support/windows_command.rs"]
mod windows_command;

pub(super) fn exec_test_timeout() -> Duration {
    Duration::from_secs(if cfg!(windows) { 15 } else { 4 })
}
pub(super) fn completed_command() -> &'static str {
    if cfg!(windows) {
        "<nul set /p=completed& cmd /c exit 0"
    } else {
        "printf completed"
    }
}
pub(super) fn gated_completion_command(root: &std::path::Path) -> Value {
    #[cfg(windows)]
    {
        // Enroll before publishing readiness and recheck after every wait.
        windows_command::powershell_file(
            root,
            "gated-completion.ps1",
            "$watcher = New-Object IO.FileSystemWatcher ((Get-Location).Path), 'release'; \
             $watcher.EnableRaisingEvents = $true; \
             Set-Content -LiteralPath ready -Value ready -NoNewline; \
             while (!(Test-Path -LiteralPath release)) { \
                 $null = $watcher.WaitForChanged([IO.WatcherChangeTypes]::Created, 1000) \
             }; $watcher.Dispose(); [Console]::Write('completed')",
        )
    }
    #[cfg(not(windows))]
    {
        let _ = root;
        json!("mkfifo release; : > ready; IFS= read -r token < release; printf completed")
    }
}
pub(super) async fn release_completion(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        // Opening a FIFO normally blocks forever if its reader has exited.
        // Nonblocking open also handles readiness preceding the shell's read.
        tokio::time::timeout(exec_test_timeout(), async {
            loop {
                match std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(path)
                {
                    Ok(mut writer) => return writer.write_all(b"release\n"),
                    Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                        tokio::task::yield_now().await;
                    }
                    Err(error) => return Err(error),
                }
            }
        })
        .await??;
    }
    #[cfg(not(unix))]
    std::fs::write(path, b"release\n")?;
    Ok(())
}
pub(super) async fn assert_completed_exec(
    fixture: &Fixture,
    tools: &ToolRegistry,
    handle: &Value,
) -> Result<()> {
    let result = tools
        .get("poll")
        .unwrap()
        .invoke(
            fixture.context(&fixture.parent),
            json!({"handle":handle,"yieldMs":exec_test_timeout().as_millis(),
                "cursor":{"stdout":0,"stderr":0}}),
        )
        .await?;
    let result: Value = serde_json::from_str(&result.content)?;
    assert_eq!(result["status"], "exited", "{result}");
    assert_eq!(result["exitCode"], 0, "{result}");
    assert_eq!(result["stdout"], "completed", "{result}");
    assert_eq!(result["stderr"], "", "{result}");
    Ok(())
}
