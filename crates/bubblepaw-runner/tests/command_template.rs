use bubblepaw_core::{CancellationToken, ToolContext, ToolRegistry};
use bubblepaw_runner::{
    command_template::{CommandExecution, prepare},
    tools::register_native_tools,
};
use serde_json::{Value, json};

#[path = "support/fixture_binary.rs"]
mod fixture_binary;

fn context() -> ToolContext {
    ToolContext {
        session_id: "owner".into(),
        run_id: "run".into(),
        invocation_id: "call".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    }
}

#[test]
fn structure_rendering_and_raw_multiline_are_preserved() {
    let template = json!({"parts":["rg --glob "," -e ",""],"values":["*.rs","a'\"$()`\n雪"]});
    let prepared = prepare(&template).unwrap();
    assert_eq!(prepared.original, template);
    match &prepared.execution {
        CommandExecution::NativeArgv { executable, args } => {
            assert_eq!(executable, "rg");
            assert_eq!(args, &["--glob", "*.rs", "-e", "a'\"$()`\n雪"]);
        }
        _ => panic!("template unexpectedly became a shell script"),
    }
    assert_eq!(prepared.metadata()["execution_mode"], "native_argv");
    assert!(!prepared.metadata().to_string().contains("雪"));
    #[cfg(windows)]
    {
        assert!(prepared.rendered.contains("\"a'\\\"$()`\n雪\""));
        assert!(!prepared.rendered.starts_with("& "));
        assert_eq!(bubblepaw_runner::command_template::shell_name(), "cmd.exe");
    }
    #[cfg(not(windows))]
    assert!(prepared.rendered.contains("'a'\"'\"'\"$()`\n雪'"));
    let raw = "first\nsecond\n\"already quoted\"; $literal `literal";
    let prepared = prepare(&json!(raw)).unwrap();
    assert_eq!(prepared.original, raw);
    assert_eq!(prepared.rendered, raw);
    match prepared.execution {
        CommandExecution::Shell { script } => assert_eq!(script, raw),
        _ => panic!("string unexpectedly became argv"),
    }
}

#[test]
fn rejects_unsupported_placement_types_and_script_contexts() {
    for input in [
        json!({"parts":["rg --glob=", ""],"values":["x"]}),
        json!({"parts":["rg ",".rs"],"values":["x"]}),
        json!({"parts":["rg '","'"],"values":["x"]}),
        json!({"parts":["rg \"","\""],"values":["x"]}),
        json!({"parts":["", " argument"],"values":["rg"]}),
        json!({"parts":["rg # ",""],"values":["x"]}),
        json!({"parts":["cat <<EOF\n", "\nEOF"],"values":["x"]}),
        json!({"parts":["rg "," | other"],"values":["x"]}),
        json!({"parts":["rg "," > out.txt"],"values":["x"]}),
        json!({"parts":["rg "," ; effect"],"values":["x"]}),
        json!({"parts":["rg ",""],"values":["\u{0}"]}),
        json!({"parts":["rg ",""],"values":[["x"]]}),
        json!({"parts":["rg ",""],"values":[3]}),
        json!({"parts":["rg ",""],"values":[]}),
        json!({"parts":["rg"],"values":[],"raw":true}),
        json!({"parts":[],"values":[]}),
        json!("\u{0}"),
    ] {
        assert!(
            prepare(&input).is_err(),
            "accepted unsupported input: {input}"
        );
    }
    #[cfg(windows)]
    for executable in [
        "echo",
        "Write-Output",
        "script.cmd",
        "script.bat",
        "script.cmd.",
    ] {
        assert!(prepare(&json!({"parts":[executable],"values":[]})).is_err());
    }
    // A program owns the meaning of its arguments. The template guarantees
    // argv boundaries, including an intentional interpreter source argument.
    assert!(prepare(&json!({"parts":["python -c ",""],"values":["print('intentional')"]})).is_ok());
}

#[cfg(windows)]
#[tokio::test]
async fn native_name_lookup_does_not_fall_back_to_a_batch_script() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("only_batch.cmd"),
        "@echo started > escaped.txt\r\n",
    )
    .unwrap();
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, directory.path(), false).unwrap();
    let error = registry
        .get("exec")
        .unwrap()
        .invoke(
            context(),
            json!({"command":{"parts":["./only_batch ",""],"values":["value"]}}),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("native executable"));
    assert!(!directory.path().join("escaped.txt").exists());
    processes.shutdown().await;
}

/// A real executable using the platform's standard native argv contract. This
/// deliberately bypasses cmd.exe line parsing and its argument quoting.
fn argv_probe(root: &std::path::Path) -> String {
    let name = if cfg!(windows) {
        "argv_probe.exe"
    } else {
        "argv_probe"
    };
    let executable = fixture_binary::resolve(
        env!("CARGO_BIN_EXE_bubblepaw-argv-probe"),
        "bubblepaw-argv-probe",
    );
    std::fs::copy(executable, root.join(name)).unwrap();
    name.to_owned()
}

#[tokio::test]
async fn native_arguments_roundtrip_adversarial_values_without_shell_effects() {
    let directory = tempfile::tempdir().unwrap();
    let name = argv_probe(directory.path());
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, directory.path(), false).unwrap();
    let values = vec![
        "space value",
        "O'Brien",
        "embedded\"quote",
        "$(effect)",
        "`effect`",
        "",
        "line\nnext",
        "雪🐾",
        "trailing\\",
        "\\\"",
        "; echo effect > escaped.txt",
        "*",
        "--option=value",
    ];
    let mut parts = vec![format!("./{name} ")];
    parts.extend((1..values.len()).map(|_| " ".to_owned()));
    parts.push(String::new());
    let output = registry
        .get("exec")
        .unwrap()
        .invoke(
            context(),
            json!({"command":{"parts":parts,"values":values},"yieldMs":0}),
        )
        .await
        .unwrap();
    let started: Value = serde_json::from_str(&output.content).unwrap();
    let done = registry
        .get("wait")
        .unwrap()
        .invoke(
            context(),
            json!({"handle":started["handle"],"timeoutMs":5000}),
        )
        .await
        .unwrap();
    let done: Value = serde_json::from_str(&done.content).unwrap();
    assert_eq!(done["status"], "exited");
    assert_eq!(done["exitCode"], 0);
    let output = registry
        .get("poll")
        .unwrap()
        .invoke(context(), json!({"handle":started["handle"]}))
        .await
        .unwrap();
    let output: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(output["stdout"], format!("{values:?}\n"));
    assert_eq!(output["stderr"], "");
    assert!(!directory.path().join("escaped.txt").exists());
    processes.shutdown().await;
}

#[tokio::test]
async fn invalid_template_cannot_start_a_literal_prefix_effect() {
    let directory = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, directory.path(), false).unwrap();
    let invalid =
        json!({"command":{"parts":["echo started > escaped.txt; rg --glob=", ""],"values":["x"]}});
    assert!(
        registry
            .get("exec")
            .unwrap()
            .invoke(context(), invalid)
            .await
            .is_err()
    );
    assert!(!directory.path().join("escaped.txt").exists());
    processes.shutdown().await;
}
