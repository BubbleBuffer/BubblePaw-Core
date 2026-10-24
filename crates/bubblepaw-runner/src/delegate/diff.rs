//! Changed files and line counts, from git tree snapshots of the working tree.
//!
//! A snapshot copies the index to a temporary file, stages everything there and
//! writes a tree object. The real index, refs and working tree are untouched; only
//! unreferenced objects are added to the repository's object store.
use serde::Serialize;
use std::path::Path;

/// Git runs as an asynchronous child in its own process tree: the delegation
/// loop must not block a runtime worker while it waits for a snapshot, and
/// dropping the delegation kills git with its descendants.
async fn git(workspace: &Path, args: &[&str], index: Option<&Path>) -> Option<String> {
    let mut command = tokio::process::Command::new("git");
    command.current_dir(workspace).args(args);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output = crate::process_output::output(&mut command).await.ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct DiffStat {
    pub files: Vec<String>,
    pub added: u64,
    pub removed: u64,
}

/// Tree id of the current working tree including untracked, unignored files.
/// None outside a git work tree.
pub async fn snapshot(workspace: &Path) -> Option<String> {
    git(workspace, &["rev-parse", "--is-inside-work-tree"], None)
        .await
        .filter(|v| v == "true")?;
    let index = git(
        workspace,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        None,
    )
    .await?;
    let temp = tempfile::tempdir().ok()?;
    let copy = temp.path().join("index");
    if Path::new(&index).exists() {
        std::fs::copy(&index, &copy).ok()?;
    }
    git(workspace, &["add", "-A", "--", "."], Some(&copy)).await?;
    git(workspace, &["write-tree"], Some(&copy)).await
}

/// Files and lines changed between two snapshots.
pub async fn between(workspace: &Path, before: &str, after: &str) -> Option<DiffStat> {
    let numstat = git(
        workspace,
        &["diff", "--numstat", "--no-renames", before, after],
        None,
    )
    .await?;
    let mut stat = DiffStat::default();
    for line in numstat.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        // Binary files report "-".
        stat.added += added.parse::<u64>().unwrap_or(0);
        stat.removed += removed.parse::<u64>().unwrap_or(0);
        stat.files.push(path.to_owned());
    }
    Some(stat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn counts_tracked_and_untracked_changes_without_touching_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        if git(root, &["init", "-q"], None).await.is_none() {
            return; // git unavailable
        }
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        git(root, &["add", "a.txt"], None).await.unwrap();
        let before = snapshot(root).await.unwrap();
        std::fs::write(root.join("a.txt"), "one\nthree\n").unwrap();
        std::fs::write(root.join("new.txt"), "x\ny\n").unwrap();
        let after = snapshot(root).await.unwrap();
        let stat = between(root, &before, &after).await.unwrap();
        assert_eq!(stat.files, ["a.txt", "new.txt"]);
        assert_eq!((stat.added, stat.removed), (3, 1));
        let status = git(root, &["status", "--porcelain"], None).await.unwrap();
        assert!(status.contains("?? new.txt"), "{status}");
        assert!(
            snapshot(&root.join("..").join("definitely-missing"))
                .await
                .is_none()
        );
    }
}
