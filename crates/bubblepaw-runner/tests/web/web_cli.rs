use serde_json::Value;
use std::{path::Path, process::Command};

fn run(root: &Path, flags: &[&str]) -> Value {
    let file = root.join("surface.js");
    std::fs::write(
        &file,
        "return {names:Object.keys(tools),agent:(await tools.$search('select:agent')).tools[0]};",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_bubblepaw"))
        .args(["--set", "agent.execution_mode='js'"])
        .args(["--provider", "demo", "--no-auto-summarize", "--workspace"])
        .arg(root)
        .arg("--db")
        .arg(root.join("archive.sqlite"))
        .args(flags)
        .arg("script")
        .arg(file)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn cli_web_is_disabled_by_default_and_enabled_surface_is_explicit() {
    for (flags, expected) in [
        (vec![], vec![]),
        (vec!["--web"], vec!["web_fetch", "web_search"]),
        (
            vec!["--web", "--web-expose", "fetch,search"],
            vec!["web_fetch", "web_search"],
        ),
        (vec!["--web", "--web-expose", "fetch"], vec!["web_fetch"]),
    ] {
        let root = tempfile::tempdir().unwrap();
        let surface = run(root.path(), &flags);
        let names = surface["names"].as_array().unwrap();
        for name in ["research", "web_fetch", "web_search"] {
            assert_eq!(
                names.contains(&Value::String(name.into())),
                expected.contains(&name),
                "{surface}"
            );
        }
        let profiles = surface["agent"]["inputSchema"]["properties"]["profile"]["enum"]
            .as_array()
            .unwrap();
        assert!(profiles.contains(&Value::String("read-only".into())));
        assert!(profiles.contains(&Value::String("workspace".into())));
        assert_eq!(
            profiles.contains(&Value::String("research".into())),
            flags.contains(&"--web")
        );
    }
}

#[test]
fn read_only_cli_has_no_workspace_profile() {
    let root = tempfile::tempdir().unwrap();
    let surface = run(root.path(), &["--web", "--read-only"]);
    assert_eq!(
        surface["agent"]["inputSchema"]["properties"]["profile"]["enum"],
        serde_json::json!(["read-only", "research"])
    );
}

#[test]
fn layered_web_opt_in_and_argument_order_control_surface() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("web.toml");
    std::fs::write(
        &config,
        "[web]\nenabled=true\n[web.expose]\nfetch=false\nsearch=true\n",
    )
    .unwrap();
    let surface = run(root.path(), &["--config", config.to_str().unwrap()]);
    let names = surface["names"].as_array().unwrap();
    assert!(names.contains(&Value::String("web_search".into())));
    assert!(!names.contains(&Value::String("research".into())));
    let surface = run(root.path(), &["--web", "--set", "web.enabled=false"]);
    assert!(
        !surface["names"]
            .as_array()
            .unwrap()
            .contains(&Value::String("web_fetch".into()))
    );
    let surface = run(
        root.path(),
        &[
            "--set",
            "web.enabled=false",
            "--web",
            "--web-expose",
            "search",
        ],
    );
    assert!(
        surface["names"]
            .as_array()
            .unwrap()
            .contains(&Value::String("web_search".into()))
    );
}

#[test]
fn removed_research_tool_configuration_is_rejected() {
    for (flags, diagnostic) in [
        (
            vec!["--research-model", "demo/research"],
            "--research-model",
        ),
        (
            vec!["--web-expose", "research"],
            "web-expose accepts fetch and search",
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_bubblepaw"))
            .args(["--set", "agent.execution_mode='js'"])
            .args(["--provider", "demo", "--no-auto-summarize", "--workspace"])
            .arg(root.path())
            .arg("--data-dir")
            .arg(root.path().join("state"))
            .args(flags)
            .args(["run", "unused"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(diagnostic),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
