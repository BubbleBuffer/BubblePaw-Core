//! Without `--data-dir`, every workspace uses one central store under the local
//! data home. The sandbox sets LOCALAPPDATA, which takes precedence on every
//! platform, so no test touches the user's real data directory.
use bubblepaw_core::Store;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn workspace(&self, name: &str) -> PathBuf {
        let path = self.root.path().join(name);
        std::fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
    }

    fn base(&self) -> PathBuf {
        self.root.path().join("local").join("BubblePaw-Core")
    }

    fn central(&self) -> PathBuf {
        self.base().join("store")
    }

    fn command(&self, workspace: &Path, args: &[&str]) -> Command {
        let home = self.root.path().join("home");
        let mut command = Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("BUBBLEPAW_") {
                command.env_remove(key);
            }
        }
        command
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENROUTER_API_KEY")
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("LOCALAPPDATA", self.root.path().join("local"))
            .env("APPDATA", home.join("roaming"))
            .env("XDG_DATA_HOME", self.root.path().join("xdg"))
            .env("CODEX_HOME", home.join("codex"))
            .current_dir(workspace)
            .args(["--provider", "demo", "--json", "--workspace"])
            .arg(workspace)
            .args(args);
        command
    }

    fn cli(&self, workspace: &Path, args: &[&str]) -> Output {
        let output = self.command(workspace, args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn session_of(output: &Output) -> String {
    stdout(output)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|record| record["type"] == "result")
        .and_then(|record| record["session_id"].as_str().map(str::to_owned))
        .expect("run result")
}

#[test]
fn workspaces_without_data_dir_share_the_central_store_and_keep_their_workspaces() {
    let sandbox = Sandbox::new();
    let first = sandbox.workspace("first");
    let second = sandbox.workspace("second");
    let a = session_of(&sandbox.cli(&first, &["run", "hello from the first workspace"]));
    let b = session_of(&sandbox.cli(&second, &["run", "hello from the second workspace"]));
    assert_ne!(a, b);
    // One store, named `store`; no per-workspace hash directory is created.
    let entries: Vec<_> = std::fs::read_dir(sandbox.base())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, ["store"]);
    let listed: Vec<Value> =
        serde_json::from_str(&stdout(&sandbox.cli(&second, &["sessions"]))).unwrap();
    let ids: Vec<_> = listed.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        ids.contains(&a.as_str()) && ids.contains(&b.as_str()),
        "{ids:?}"
    );
    let store = Store::open_data_dir(sandbox.central()).unwrap();
    assert_eq!(
        store.session_workspace(&a).unwrap().as_deref(),
        Some(first.as_path())
    );
    assert_eq!(
        store.session_workspace(&b).unwrap().as_deref(),
        Some(second.as_path())
    );
}
