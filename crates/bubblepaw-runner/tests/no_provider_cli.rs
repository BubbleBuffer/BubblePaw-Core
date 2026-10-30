//! `bubblepaw run` on a first run: the host starts, then the run is refused
//! with an error that tells the CLI user how to choose a provider.
use std::process::Command;

#[test]
fn run_without_a_provider_fails_with_a_clear_error() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(root.path().join("empty.toml"), "").unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy().into_owned();
        if name.starts_with("BUBBLEPAW_") || name.ends_with("_API_KEY") {
            command.env_remove(name);
        }
    }
    let output = command
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("LOCALAPPDATA", home.join("local"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("CODEX_HOME", home.join("codex"))
        .current_dir(&workspace)
        .arg("--config")
        .arg(root.path().join("empty.toml"))
        .arg("--data-dir")
        .arg(root.path().join("data"))
        .args(["run", "hello"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("no model provider is configured"),
        "{stderr}"
    );
    assert!(stderr.contains("--provider"), "{stderr}");
    assert!(stderr.contains("providers.save"), "{stderr}");
}
