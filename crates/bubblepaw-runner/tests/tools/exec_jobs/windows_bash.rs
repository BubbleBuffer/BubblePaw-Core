//! Windows exec under Git for Windows' bash and under cmd.exe (docs/exec.md).
//! Skipped quietly on a machine without Git for Windows.
use super::*;
use bubblepaw_core::{Harness, HarnessConfig, Role, RunOptions, Store, ToolCall};
use bubblepaw_runner::provider::{ScriptedProvider, ScriptedStep};

fn shell(name: &str) -> ExecConfig {
    ExecConfig {
        shell: name.into(),
        ..ExecConfig::default()
    }
}

fn have_git_bash() -> bool {
    bubblepaw_runner::exec_shell::resolve("bash").is_ok()
}

async fn run(fixture: &Fixture, command: &str) -> Value {
    call(
        &fixture.registry,
        "owner",
        "exec",
        json!({"command":command}),
    )
    .await
}

#[tokio::test]
async fn forced_bash_runs_bash_only_commands_and_keeps_quotes_and_the_cwd_note() {
    if !have_git_bash() {
        return;
    }
    let fixture = Fixture::new(shell("bash"));
    let piped = run(&fixture, "set -e; printf x | tail -c 1").await;
    assert_eq!(piped["exitCode"], 0, "{piped}");
    assert_eq!(piped["stdout"], "x", "{piped}");
    let quoted = run(&fixture, r#"printf '%s' "x\"y" 'a\b' *"#).await;
    assert_eq!(quoted["stdout"], r#"x"ya\b*"#, "{quoted}");
    let moved = run(&fixture, "mkdir sub && cd sub && pwd -W").await;
    assert!(moved["stdout"].as_str().unwrap().contains("sub"), "{moved}");
    assert!(
        moved["note"].as_str().unwrap_or_default().contains("sub"),
        "{moved}"
    );
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn forced_cmd_behaves_as_before() {
    let fixture = Fixture::new(shell("cmd"));
    let echoed = run(&fixture, ECHO).await;
    assert_eq!(echoed["stdout"], "hello", "{echoed}");
    assert_eq!(echoed["exitCode"], 0, "{echoed}");
    fixture.processes.shutdown().await;
}

#[tokio::test]
async fn hardline_commands_are_refused_under_bash_before_any_shell_starts() {
    if !have_git_bash() {
        return;
    }
    let fixture = Fixture::new(shell("bash"));
    let victim = fixture._workspace.path().canonicalize().unwrap();
    let plain = victim
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('\\', "/");
    let msys = format!("/{}{}", plain[..1].to_lowercase(), &plain[2..]);
    for command in [format!("rm -rf '{msys}'"), format!("rm -rf \"{plain}\"")] {
        let store = Store::in_memory().unwrap();
        store.bind_workspace(victim.as_path()).unwrap();
        let session = store.create_session("hardline").unwrap();
        let provider = ScriptedProvider::with_steps(
            vec![ScriptedStep::Calls {
                calls: vec![ToolCall {
                    id: "c".into(),
                    name: "exec".into(),
                    arguments: json!({"command": command}),
                }],
            }],
            "done",
        );
        let mut tools = ToolRegistry::new();
        fixture
            .processes
            .register_without_store(&mut tools)
            .unwrap();
        let harness = Harness::new(
            store.clone(),
            Arc::new(provider),
            tools,
            HarnessConfig::default(),
        );
        harness
            .prompt(&session.id, "go", RunOptions::default())
            .await
            .unwrap();
        let refused = store
            .messages(&session.id)
            .unwrap()
            .into_iter()
            .find(|m| m.role == Role::Tool)
            .unwrap();
        assert!(
            refused.is_error && refused.text().contains("hardline"),
            "{command}: {}",
            refused.text()
        );
        assert!(victim.exists());
    }
    fixture.processes.shutdown().await;
}
