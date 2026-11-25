//! Short helper commands (git queries and snapshots) run to completion in their
//! own process tree, like exec and delegate checks.
use std::{io, process::Output, process::Stdio, time::Duration};
use tokio::process::Command;

/// Runs `command` with piped output and waits for it. The command owns a
/// process tree: when this future finishes or is dropped (the caller was
/// cancelled), the command and every descendant are killed.
pub(crate) async fn output(command: &mut Command) -> io::Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let (child, tree) = bubblepaw_process::spawn(command)?;
    let output = child.wait_with_output().await;
    tree.kill();
    output
}

/// [`output`] for synchronous callers, with a time limit. The command runs on a
/// helper thread with its own runtime, so this is safe to call from inside or
/// outside a runtime. `Ok(None)` means the limit passed: the future was dropped
/// and with it the command's whole process tree.
pub(crate) fn output_blocking(mut command: Command, limit: Duration) -> io::Result<Option<Output>> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                runtime.block_on(async {
                    match tokio::time::timeout(limit, output(&mut command)).await {
                        Ok(output) => output.map(Some),
                        Err(_) => Ok(None),
                    }
                })
            })
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("git helper thread panicked")))
    })
}

/// Runs `program -C dir args` (git) to completion for synchronous callers.
/// Stdin is closed and `GIT_TERMINAL_PROMPT=0`, so git never waits for input;
/// past `limit` the process tree is killed and the error names the command.
pub(crate) fn git_output(
    program: &str,
    dir: &std::path::Path,
    args: &[&str],
    limit: Duration,
) -> anyhow::Result<Output> {
    use anyhow::Context;
    let mut command = Command::new(program);
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    output_blocking(command, limit)
        .with_context(|| {
            format!(
                "could not start git in {}; is git installed?",
                dir.display()
            )
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "git {} timed out after {}s in {} and was killed",
                args.join(" "),
                limit.as_secs_f32(),
                dir.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_command_that_hangs_is_killed_at_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();
        let error = git_output(
            "git",
            dir.path(),
            &["-c", "alias.hang=!sleep 60", "hang"],
            Duration::from_millis(500),
        )
        .expect_err("the hanging command must time out");
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(30));
    }
}
