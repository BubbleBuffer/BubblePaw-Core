//! `bubblepaw trust [PATH]`, `--revoke` and `--list`: the user-level record of trusted workspaces.
use crate::{cli::TrustArgs, workspace_trust};
use anyhow::{Context, Result, ensure};
use std::path::Path;

/// Run the command against the invoking user's home. Nothing here reads the workspace.
pub fn run(args: &TrustArgs, workspace: &Path) -> Result<()> {
    let home = workspace_trust::default_home().context("cannot find the home directory")?;
    run_in(args, workspace, &home)
}

pub(crate) fn run_in(args: &TrustArgs, workspace: &Path, home: &Path) -> Result<()> {
    if args.list {
        for record in workspace_trust::list(home)? {
            println!("{}\t{}", record.trusted_at, record.path);
        }
        return Ok(());
    }
    let target = args.path.as_deref().unwrap_or(workspace);
    ensure!(target.is_dir(), "{} is not a directory", target.display());
    let root = workspace_trust::canonical(target);
    if args.revoke {
        let removed = workspace_trust::untrust(home, &root)?;
        println!(
            "{} {}",
            if removed {
                "No longer trusted:"
            } else {
                "Was not trusted:"
            },
            root.display()
        );
    } else {
        let record = workspace_trust::trust(home, &root)?;
        println!("Trusted {} (since {})", record.path, record.trusted_at);
    }
    Ok(())
}
