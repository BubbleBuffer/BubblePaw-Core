//! Reports the child worktrees that a cleanup pass removed or kept.
use anyhow::anyhow;
use bubblepaw_core::Store;

/// Cleanup runs git and file I/O synchronously, so it goes to the blocking pool.
pub(super) async fn report_worktree_cleanup(store: &Store, output: &crate::cli_output::Output) {
    if store.data_dir().is_none() {
        return;
    }
    let cleaning = store.clone();
    let cleaned =
        tokio::task::spawn_blocking(move || crate::worktrees::cleanup_inactive(&cleaning))
            .await
            .unwrap_or_else(|error| Err(anyhow!("worktree cleanup task failed: {error}")));
    match cleaned {
        Ok(reports) => {
            for report in reports {
                let _ = output.stderr(format!(
                    "Child worktree {}: {}\n",
                    report.session_id, report.note
                ));
            }
        }
        Err(error) => {
            let _ = output.stderr(format!(
                "Child worktree cleanup retained workspaces: {error:#}\n"
            ));
        }
    }
}
