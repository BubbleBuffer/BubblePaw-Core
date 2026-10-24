//! Acceptance checks run by the harness in the workspace, with the exec tool's shell.
use bubblepaw_core::CancellationToken;
use serde::Serialize;
use std::{
    io,
    path::Path,
    process::{ExitStatus, Stdio},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    task::JoinHandle,
};

/// Bytes kept from the end of each output stream.
pub const TAIL_BYTES: usize = 2048;
/// Longest a single check may run; a check past it is killed and fails.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CheckResult {
    pub command: String,
    /// None when the command could not be started or ended by a signal.
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// Bounded tail of stdout then stderr, or the spawn error.
    pub tail: String,
    /// The check was killed at [`CHECK_TIMEOUT`].
    pub timed_out: bool,
}
impl CheckResult {
    pub fn passed(&self) -> bool {
        self.exit_code == Some(0)
    }
    /// One verified line for the dispatcher.
    pub fn line(&self) -> String {
        let outcome = match self.exit_code {
            _ if self.timed_out => "fail (timeout)".to_owned(),
            Some(0) => "pass".to_owned(),
            Some(code) => format!("fail (exit {code})"),
            None => "fail (no exit code)".to_owned(),
        };
        format!(
            "{outcome} {:.1}s: {}",
            self.duration_ms as f64 / 1000.0,
            self.command
        )
    }
}

pub fn tail(bytes: &[u8], limit: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_end();
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[{start} bytes omitted]\n{}", &text[start..])
}

fn shell(script: &str) -> tokio::process::Command {
    #[cfg(windows)]
    let mut command = {
        let mut c = tokio::process::Command::new("powershell.exe");
        c.args(["-NoProfile", "-NonInteractive", "-Command", script]);
        c
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", script]);
        c
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// Run one check in `workspace`, killed after [`CHECK_TIMEOUT`].
pub async fn run(workspace: &Path, command: &str) -> CheckResult {
    // A token nothing cancels, so the check always ends with a result.
    run_within(
        workspace,
        command,
        CHECK_TIMEOUT,
        &CancellationToken::default(),
        None,
    )
    .await
    .expect("an uncancelled check always has a result")
}

enum Ended {
    Exited(io::Result<ExitStatus>),
    TimedOut,
    Cancelled,
}

/// Run one check until it ends, times out or `cancellation` fires. A timed-out or cancelled
/// check has its whole process group killed; a cancelled check returns None.
async fn run_within(
    workspace: &Path,
    command: &str,
    limit: Duration,
    cancellation: &CancellationToken,
    environment: Option<&[(String, String)]>,
) -> Option<CheckResult> {
    let started = Instant::now();
    let mut process = shell(command);
    process.current_dir(workspace);
    if let Some(environment) = environment {
        process.env_clear().envs(environment.iter().cloned());
    }
    // The check runs in its own process tree, so a kill also reaches the
    // children the script started, on both OSes.
    let (mut child, tree) = match bubblepaw_process::spawn(&mut process) {
        Ok(child) => child,
        Err(error) => {
            return Some(CheckResult {
                command: command.to_owned(),
                exit_code: None,
                duration_ms: started.elapsed().as_millis() as u64,
                tail: format!("could not start: {error}"),
                timed_out: false,
            });
        }
    };
    // Both streams are read as they arrive; only their bounded tails are kept.
    let stdout_task = tokio::spawn(read_tail(child.stdout.take()));
    let stderr_task = tokio::spawn(read_tail(child.stderr.take()));
    let wait = child.wait();
    tokio::pin!(wait);
    let ended = tokio::select! {
        status = &mut wait => Ended::Exited(status),
        () = tokio::time::sleep(limit) => Ended::TimedOut,
        () = cancellation.cancelled() => Ended::Cancelled,
    };
    // Kill the tree before draining: a descendant that inherited an output pipe
    // would otherwise keep the readers waiting after the script itself exited.
    tree.kill();
    // Keep the child future alive until its process tree is killed and reaped.
    if !matches!(ended, Ended::Exited(_)) {
        let _ = tokio::time::timeout(Duration::from_secs(3), &mut wait).await;
    }
    let stdout = finish_tail(stdout_task).await;
    let stderr = finish_tail(stderr_task).await;
    let duration_ms = started.elapsed().as_millis() as u64;
    match ended {
        Ended::TimedOut => Some(CheckResult {
            command: command.to_owned(),
            exit_code: None,
            duration_ms,
            tail: format!("timeout after {}s", limit.as_secs()),
            timed_out: true,
        }),
        Ended::Cancelled => None,
        Ended::Exited(Err(error)) => Some(CheckResult {
            command: command.to_owned(),
            exit_code: None,
            duration_ms,
            tail: format!("could not wait: {error}"),
            timed_out: false,
        }),
        Ended::Exited(Ok(status)) => {
            let mut text = tail(&stdout, TAIL_BYTES);
            let stderr = tail(&stderr, TAIL_BYTES);
            if !stderr.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&stderr);
            }
            Some(CheckResult {
                command: command.to_owned(),
                exit_code: status.code(),
                duration_ms,
                tail: text,
                timed_out: false,
            })
        }
    }
}

/// Reads one stream to its end, keeping only the last TAIL_BYTES bytes, so a
/// check that prints megabytes costs a bounded amount of memory.
async fn read_tail<R: AsyncRead + Unpin>(reader: Option<R>) -> Vec<u8> {
    let Some(mut reader) = reader else {
        return Vec::new();
    };
    let mut kept = Vec::with_capacity(TAIL_BYTES);
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => {
                kept.extend_from_slice(&chunk[..read]);
                if kept.len() > TAIL_BYTES {
                    let excess = kept.len() - TAIL_BYTES;
                    kept.drain(..excess);
                }
            }
            Err(_) => break,
        }
    }
    kept
}

/// A reader that does not finish is abandoned rather than holding the check.
async fn finish_tail(task: JoinHandle<Vec<u8>>) -> Vec<u8> {
    match tokio::time::timeout(Duration::from_secs(5), task).await {
        Ok(Ok(kept)) => kept,
        _ => Vec::new(),
    }
}

/// Run every check in order; later checks still run after a failure.
pub async fn run_all(workspace: &Path, commands: &[String]) -> Vec<CheckResult> {
    run_all_until(workspace, commands, &CancellationToken::default()).await
}

/// Run every check in order until `cancellation` fires. The check it interrupts is killed
/// with its process tree and not returned, and no later check starts, so fewer results than
/// commands means the checks were cancelled.
pub async fn run_all_until(
    workspace: &Path,
    commands: &[String],
    cancellation: &CancellationToken,
) -> Vec<CheckResult> {
    run_all_with_environment(workspace, commands, cancellation, None).await
}

pub async fn run_all_with_environment(
    workspace: &Path,
    commands: &[String],
    cancellation: &CancellationToken,
    environment: Option<&[(String, String)]>,
) -> Vec<CheckResult> {
    let mut results = Vec::with_capacity(commands.len());
    for command in commands {
        if cancellation.is_cancelled() {
            break;
        }
        match run_within(workspace, command, CHECK_TIMEOUT, cancellation, environment).await {
            Some(result) => results.push(result),
            None => break,
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_is_bounded_on_a_char_boundary() {
        let text = "é".repeat(3000);
        let out = tail(text.as_bytes(), 101);
        assert!(out.starts_with('['));
        assert!(out.len() < 140);
        assert_eq!(tail(b"short\n", 100), "short");
    }

    #[tokio::test]
    async fn records_exit_code_duration_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let ok = run(dir.path(), "echo hello").await;
        assert!(ok.passed(), "{ok:?}");
        assert!(ok.tail.contains("hello"));
        let failed = run(dir.path(), "exit 3").await;
        assert_eq!(failed.exit_code, Some(3));
        assert!(failed.line().starts_with("fail (exit 3)"));
        assert!(!failed.timed_out);
    }

    #[tokio::test]
    async fn several_megabytes_of_output_keep_only_a_bounded_tail() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        let noisy = "1..200000 | ForEach-Object { '0123456789abcdefghij' }";
        #[cfg(not(windows))]
        let noisy = "awk 'BEGIN { for (i = 0; i < 200000; i++) print \"0123456789abcdefghij\" }'";
        let result = run(dir.path(), noisy).await;
        assert!(result.passed(), "{result:?}");
        // Only one bounded tail per stream is retained, not the whole output.
        assert!(
            result.tail.len() <= TAIL_BYTES + 64,
            "kept {} tail bytes",
            result.tail.len()
        );
        assert!(result.tail.ends_with("0123456789abcdefghij"), "{result:?}");
    }

    #[tokio::test]
    async fn a_check_past_the_limit_is_killed_and_fails_as_timeout() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        let slow = "Start-Sleep -Seconds 30";
        #[cfg(not(windows))]
        let slow = "sleep 30";
        let result = run_within(
            dir.path(),
            slow,
            Duration::from_millis(300),
            &CancellationToken::default(),
            None,
        )
        .await
        .unwrap();
        assert!(result.timed_out);
        assert!(!result.passed());
        assert!(result.duration_ms < 10_000, "{result:?}");
        assert!(result.line().starts_with("fail (timeout)"));
        assert!(result.tail.contains("timeout"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn checks_use_the_client_environment_without_host_values() {
        let dir = tempfile::tempdir().unwrap();
        let environment = vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("ATTACH_CHECK".into(), "client".into()),
        ];
        let commands = vec!["test \"$ATTACH_CHECK\" = client && test -z \"${HOME+x}\"".to_owned()];
        let results = run_all_with_environment(
            dir.path(),
            &commands,
            &CancellationToken::default(),
            Some(&environment),
        )
        .await;
        assert_eq!(results.len(), 1);
        assert!(results[0].passed(), "{results:?}");
    }

    #[tokio::test]
    async fn a_cancelled_check_ends_without_a_result_and_stops_later_checks() {
        let dir = tempfile::tempdir().unwrap();
        // The check marks that it started, so cancelling hits it mid-run.
        #[cfg(windows)]
        let slow = "Set-Content -LiteralPath started -Value ''; Start-Sleep -Seconds 30";
        #[cfg(not(windows))]
        let slow = ": > started; sleep 30";
        let token = CancellationToken::new();
        let canceller = token.clone();
        let started_marker = dir.path().join("started");
        let cancel = async move {
            tokio::time::timeout(Duration::from_secs(10), async {
                while !started_marker.exists() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the slow check never started");
            canceller.cancel();
        };
        let commands = vec![slow.to_owned(), "echo later > later.txt".to_owned()];
        let started = Instant::now();
        let (results, ()) = tokio::join!(run_all_until(dir.path(), &commands, &token), cancel);
        assert!(results.is_empty(), "{results:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(!dir.path().join("later.txt").exists());
    }
}
