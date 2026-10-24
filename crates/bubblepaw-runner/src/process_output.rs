//! Short helper commands (git queries and snapshots) run to completion in their
//! own process tree, like exec and delegate checks.
use std::{io, process::Output, process::Stdio};
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
