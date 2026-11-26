//! The extension provider as the host wires it: the extensions file, and the
//! launcher that starts each extension in a process tree the host can kill. The
//! provider and its protocol live in `bubblepaw-ai`.
use anyhow::{Context as _, Result};
use bubblepaw_ai::extension::{
    ExtensionConfig, ExtensionLauncher, ExtensionProvider, Launched, LaunchedProcess,
};
use bubblepaw_process::{ProcessTree, spawn};
use std::{io, path::Path, sync::Arc};
use tokio::process::{Child, Command};

/// Starts extensions with `bubblepaw_process::spawn`, so killing or dropping one
/// kills everything it started.
struct TreeLauncher;
impl ExtensionLauncher for TreeLauncher {
    fn launch(&self, command: &mut Command) -> io::Result<Launched> {
        let (mut child, tree) = spawn(command)?;
        let missing = |pipe: &str| io::Error::other(format!("extension {pipe}"));
        let stdin = child.stdin.take().ok_or_else(|| missing("stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| missing("stderr"))?;
        Ok(Launched {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            stderr: Box::new(stderr),
            process: Box::new(Tree {
                _process: child,
                tree,
            }),
        })
    }
}
/// The spawned process and its tree. Dropping it kills the process, then the tree.
struct Tree {
    _process: Child,
    tree: ProcessTree,
}
impl LaunchedProcess for Tree {
    fn kill_tree(&self) {
        self.tree.kill();
    }
}

/// An extension provider that starts its process in a kill-tree group.
pub fn extension_provider(config: ExtensionConfig) -> ExtensionProvider {
    ExtensionProvider::new(config, Arc::new(TreeLauncher))
}

/// Whether `name` can be set on a subprocess and listed in `EXTENSION_ENV_KEYS`.
pub fn valid_env_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['=', ',', '\0'])
}

/// Load an extensions file. A relative `cwd` is resolved against the file's directory.
pub fn load_extensions(path: impl AsRef<Path>) -> Result<Vec<ExtensionConfig>> {
    let path = path.as_ref();
    let bytes = std::fs::read(path)
        .with_context(|| format!("cannot read extensions file {}", path.display()))?;
    let mut entries: Vec<ExtensionConfig> = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid extensions file {}", path.display()))?;
    let base = path.parent().unwrap_or(Path::new("."));
    for entry in &mut entries {
        anyhow::ensure!(
            !entry.id.is_empty() && !entry.command.is_empty(),
            "extension entries need an id and a command"
        );
        anyhow::ensure!(
            entry.env.keys().all(|name| valid_env_name(name)),
            "extension env names must be nonempty and contain no '=', ',' or NUL"
        );
        if let Some(cwd) = &entry.cwd
            && cwd.is_relative()
        {
            entry.cwd = Some(base.join(cwd));
        }
    }
    Ok(entries)
}
