//! Scripted host-dispatch checks; no shell or live model is invoked.
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    ApprovalMode, ApprovalPolicy, CancellationToken, ExecutionMode, Harness, HarnessConfig, Role,
    RunOptions, Store, Tool, ToolCall, ToolContext, ToolDefinition, ToolDispatcher, ToolRegistry,
    ToolResult,
};
use bubblepaw_runner::{
    provider::{ScriptedProvider, ScriptedStep},
    tools::{WorkspaceTools, register_native_tools},
};
use serde_json::{Value, json};
use std::{fs, path::Path, sync::Arc};

struct Bridge(&'static str);
#[async_trait]
impl Tool for Bridge {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bridge".into(),
            description: "Fixture host dispatcher entrypoint".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        unreachable!("the harness supplies its dispatcher")
    }
    async fn invoke_with_dispatcher(
        &self,
        context: ToolContext,
        input: Value,
        dispatcher: ToolDispatcher,
    ) -> Result<ToolResult> {
        dispatcher.invoke(context, self.0, input).await
    }
}

fn registry(root: &Path, store: &Store) -> Result<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    WorkspaceTools::new(root)?.register(&mut registry, false, store)?;
    Ok(registry)
}

async fn dispatched(
    store: &Store,
    registry: &ToolRegistry,
    target: &'static str,
    input: Value,
) -> Result<ToolResult> {
    dispatched_with(store, registry, target, input, HarnessConfig::default()).await
}

async fn dispatched_with(
    store: &Store,
    registry: &ToolRegistry,
    target: &'static str,
    input: Value,
    config: HarnessConfig,
) -> Result<ToolResult> {
    let mut tools = registry.clone();
    tools.register(Arc::new(Bridge(target)))?;
    let session = store.create_session("tool defaults")?;
    let provider = ScriptedProvider::with_steps(
        vec![ScriptedStep::Calls {
            calls: vec![ToolCall {
                id: "fixture-call".into(),
                name: "bridge".into(),
                arguments: input,
            }],
        }],
        "done",
    );
    let harness = Harness::new(store.clone(), Arc::new(provider), tools, config);
    let run = harness
        .prompt(&session.id, "run the fixture", RunOptions::default())
        .await?;
    ensure!(run.status == "completed", "fixture failed: {:?}", run.error);
    let message = store
        .messages(&session.id)?
        .into_iter()
        .find(|message| message.role == Role::Tool)
        .context("missing tool result")?;
    ensure!(store.pending_effects(&session.id)?.is_empty());
    Ok(if message.is_error {
        ToolResult::error(message.text())
    } else {
        ToolResult::text(message.text())
    })
}

#[tokio::test]
async fn read_returns_file_text_with_a_final_note_only_when_partial() -> Result<()> {
    let root = tempfile::tempdir()?;
    let text: String = (1..=2200).map(|line| format!("{line}\n")).collect();
    let wide: String = (1..=200)
        .map(|line| format!("{line:04} {}\n", "x".repeat(595)))
        .collect();
    for (name, body) in [
        ("lines.txt", text.as_str()),
        ("small.txt", "one\ntwo\n"),
        ("empty.txt", ""),
        ("wide.txt", wide.as_str()),
    ] {
        fs::write(root.path().join(name), body)?;
    }
    let store = Store::in_memory()?;
    let registry = registry(root.path(), &store)?;
    let first_1000: String = (1..=1000).map(|line| format!("{line}\n")).collect();
    for (input, expected) in [
        (json!({"path":"small.txt"}), "one\ntwo\n".to_owned()),
        (json!({"path":"empty.txt"}), "[empty file]".to_owned()),
        (
            json!({"path":"small.txt","startLine":2,"lineCount":1}),
            "two\n[lines 2-2 of 2]".to_owned(),
        ),
        (
            json!({"path":"small.txt","startLine":3}),
            "[no lines: file has 2 lines]".to_owned(),
        ),
        (
            json!({"path":"lines.txt"}),
            format!("{first_1000}[lines 1-1000 of 2200; continue with startLine=1001]"),
        ),
        (
            json!({"path":"lines.txt","lineCount":3000}),
            format!(
                "{}[lines 1-2000 of 2200; continue with startLine=2001]",
                (1..=2000)
                    .map(|line| format!("{line}\n"))
                    .collect::<String>()
            ),
        ),
        (
            json!({"path":"lines.txt","startLine":2001,"lineCount":3000}),
            format!(
                "{}[lines 2001-2200 of 2200]",
                (2001..=2200)
                    .map(|line| format!("{line}\n"))
                    .collect::<String>()
            ),
        ),
    ] {
        let result = dispatched(&store, &registry, "read", input.clone()).await?;
        assert!(!result.is_error, "{}", result.body());
        assert_eq!(result.body(), expected, "{input}");
    }
    // A model call through call_tool gets the same text.
    let native = registry.with_execution_mode(ExecutionMode::Native);
    let wrapped = json!({"name":"read","arguments":{"path":"small.txt"}});
    let result = dispatched(&store, &native, "call_tool", wrapped).await?;
    assert_eq!(result.body(), "one\ntwo\n");
    // Over the result limit the text stops at a line boundary and the note names
    // the last returned line.
    let result = dispatched(&store, &registry, "read", json!({"path":"wide.txt"})).await?;
    let text = result.body();
    let (body, note) = text.rsplit_once('\n').context("missing note")?;
    let returned = body.lines().count();
    assert!(returned > 50 && returned < 200 && result.body().len() < 60 * 1024);
    assert!(body.ends_with(&"x".repeat(595)));
    assert_eq!(
        note,
        format!(
            "[lines 1-{returned} of 200; continue with startLine={}]",
            returned + 1
        )
    );
    Ok(())
}

#[tokio::test]
async fn missing_workspace_paths_name_the_requested_path() -> Result<()> {
    let root = tempfile::tempdir()?;
    let missing_root = root.path().join("missing-workspace-root");
    let error = WorkspaceTools::new(&missing_root)
        .err()
        .context("missing root accepted")?;
    assert!(
        error
            .to_string()
            .contains(&missing_root.display().to_string())
    );
    let store = Store::in_memory()?;
    let registry = registry(root.path(), &store)?;
    for (target, input) in [
        ("read", json!({"path":"missing/named-file.txt"})),
        ("search", json!({"path":"missing/named-directory"})),
        (
            "search",
            json!({"path":"missing/named-search-root","query":"needle"}),
        ),
    ] {
        let requested = input["path"].as_str().unwrap().to_owned();
        let result = dispatched(&store, &registry, target, input).await?;
        assert!(result.is_error, "{target} unexpectedly succeeded");
        assert!(result.text_has(&requested), "{}", result.body());
    }
    Ok(())
}

#[tokio::test]
async fn unknown_host_arguments_list_valid_names_before_invocation() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, root.path(), false, &store)?;
    for (target, mut input) in [
        ("read", json!({"path":"missing.txt"})),
        ("search", json!({})),
        ("search", json!({"query":"needle"})),
        (
            "edit",
            json!({"path":"missing.txt","old_string":"before","new_string":"after"}),
        ),
        (
            "write",
            json!({"path":"missing.txt","content":"must not write"}),
        ),
        ("exec", json!({"command":"must never execute"})),
        ("poll", json!({"handle":"missing"})),
        ("cancel", json!({"handle":"missing"})),
        ("wait", json!({"ms":0})),
        ("jobs", json!({})),
    ] {
        input["unknownArgument"] = json!("private-value");
        let definition = registry.get_host(target).unwrap().definition();
        let fields: Vec<_> = definition.input_schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let valid = if fields.is_empty() {
            "(none)".to_owned()
        } else {
            fields.join(", ")
        };
        let result = dispatched(&store, &registry, target, input).await?;
        assert!(result.is_error, "{target} unexpectedly succeeded");
        assert!(
            result.text_has("unexpected arguments: unknownArgument"),
            "{}",
            result.body()
        );
        assert!(
            result.text_has(&format!("Valid arguments: {valid}.")),
            "{}",
            result.body()
        );
        assert!(!result.text_has("private-value"), "{}", result.body());
    }
    let result = dispatched(
        &store,
        &registry,
        "poll",
        json!({"handle":"missing","cursor":{"typo":"private-value"}}),
    )
    .await?;
    assert!(result.is_error);
    assert!(result.text_has("poll at /cursor"), "{}", result.body());
    assert!(
        result.text_has("Valid arguments: stderr, stdout."),
        "{}",
        result.body()
    );
    assert!(!result.text_has("private-value"));
    processes.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn direct_workspace_invocation_names_valid_arguments() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let registry = registry(root.path(), &store)?;
    let error = registry
        .get_host("read")
        .unwrap()
        .invoke(
            ToolContext {
                session_id: "direct-test".into(),
                run_id: "direct-run".into(),
                invocation_id: "direct-call".into(),
                cancellation: CancellationToken::new(),
                extensions: Default::default(),
            },
            json!({"path":"missing.txt","typo":"private-value"}),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("unexpected field typo"), "{error}");
    assert!(
        error.contains("valid arguments: path, startLine, lineCount"),
        "{error}"
    );
    assert!(!error.contains("private-value"));
    Ok(())
}

const GITIGNORE: &str = "ignored/\n*.log\n";
const LIB: &str = "fn needle() {}\n// two\n";

#[tokio::test]
async fn search_lists_and_finds_through_one_tool_and_notes_what_the_cap_left_out() -> Result<()> {
    let root = tempfile::tempdir()?;
    fs::create_dir_all(root.path().join("src"))?;
    fs::create_dir_all(root.path().join("ignored"))?;
    for (name, body) in [
        (".gitignore", GITIGNORE),
        ("src/lib.rs", LIB),
        ("src/notes.txt", "plain\n"),
        ("ignored/hidden.rs", "fn needle() {}\n"),
        ("build.log", "needle\n"),
    ] {
        fs::write(root.path().join(name), body)?;
    }
    let store = Store::in_memory()?;
    let registry = registry(root.path(), &store)?;
    let gitignore = format!(".gitignore ({} B)\n", GITIGNORE.len());
    let lib = format!("src/lib.rs ({} B)\n", LIB.len());
    let notes = "src/notes.txt (6 B)\n";
    let more = "[2 more files not shown; narrow with path, glob or pattern, or raise maxResults]";
    for (input, expected) in [
        (json!({}), format!("{gitignore}{lib}{notes}")),
        (json!({"glob":"*.rs"}), lib.clone()),
        (json!({"glob":["*.rs","*.txt"]}), format!("{lib}{notes}")),
        (json!({"glob":"!*.txt"}), format!("{gitignore}{lib}")),
        (json!({"path":"src"}), format!("{lib}{notes}")),
        (json!({"maxResults":1}), format!("{gitignore}{more}")),
        (
            json!({"pattern":"needle"}),
            "src/lib.rs:1: fn needle() {}\n".to_owned(),
        ),
        (
            json!({"pattern":"needle","context":1}),
            "src/lib.rs:1: fn needle() {}\nsrc/lib.rs-2- // two\n".to_owned(),
        ),
        (
            json!({"pattern":"n.edle","regex":true,"filesOnly":true}),
            lib.clone(),
        ),
        (json!({"pattern":"absent"}), "[no matches]".to_owned()),
        // `query` is the older name of `pattern`; an empty pattern lists files.
        (
            json!({"query":"needle"}),
            "src/lib.rs:1: fn needle() {}
"
            .to_owned(),
        ),
        (json!({"pattern":""}), format!("{gitignore}{lib}{notes}")),
        (json!({"query":""}), format!("{gitignore}{lib}{notes}")),
    ] {
        let result = dispatched(&store, &registry, "search", input.clone()).await?;
        assert!(!result.is_error, "{input}: {}", result.body());
        assert_eq!(result.body(), expected, "{input}");
    }
    Ok(())
}

#[tokio::test]
async fn read_takes_several_paths_and_names_what_the_size_cap_cut() -> Result<()> {
    let root = tempfile::tempdir()?;
    let wide: String = (1..=200)
        .map(|line| format!("{line:04} {}\n", "x".repeat(595)))
        .collect();
    for (name, body) in [
        ("small.txt", "one\ntwo\n"),
        ("other.txt", "a\nb\nc\n"),
        ("wide.txt", wide.as_str()),
    ] {
        fs::write(root.path().join(name), body)?;
    }
    let store = Store::in_memory()?;
    let registry = registry(root.path(), &store)?;
    let read = |input: Value| dispatched(&store, &registry, "read", input);
    let result = read(json!({"paths":["small.txt",{"path":"other.txt","startLine":2}]})).await?;
    assert_eq!(
        result.body(),
        "=== small.txt ===\none\ntwo\n=== other.txt ===\nb\nc\n[lines 2-3 of 3]\n"
    );
    let result = read(json!({"paths":["small.txt","missing.txt"]})).await?;
    assert!(!result.is_error);
    let text = result.body();
    assert!(text.starts_with("=== small.txt ===\none\ntwo\n=== missing.txt ===\n[error: "));
    assert!(text.contains("missing.txt"), "{text}");
    let result = read(json!({"paths":["wide.txt","small.txt","other.txt"]})).await?;
    let text = result.body();
    assert!(text.len() < 60 * 1024, "{}", text.len());
    assert!(text.starts_with("=== wide.txt ===\n0001 "));
    assert!(text.contains("[cut at the total size cap; read this file alone with startLine="));
    assert!(text.contains("Cut: wide.txt."));
    assert!(text.contains("Not read: small.txt, other.txt."));
    Ok(())
}

#[tokio::test]
async fn exec_string_command_reports_its_exit_code_and_keeps_the_hardline() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, root.path(), false, &store)?;
    let run = |command: &str| {
        let config = HarnessConfig {
            approval: ApprovalPolicy {
                confirm: ApprovalMode::Allow,
                ..Default::default()
            },
            ..Default::default()
        };
        dispatched_with(
            &store,
            &registry,
            "exec",
            json!({"command":command}),
            config,
        )
    };
    let ok = run("exit 0").await?;
    assert!(!ok.is_error, "{}", ok.body());
    assert_eq!(serde_json::from_str::<Value>(&ok.body())?["exitCode"], 0);
    let failed = run("exit 7").await?;
    assert!(failed.is_error, "{}", failed.body());
    assert_eq!(
        serde_json::from_str::<Value>(&failed.body())?["exitCode"],
        7
    );
    assert!(failed.text_has("code 7"), "{}", failed.body());
    // A string command passes the same hardline floor as its `sh -c` form.
    for command in [
        "mkfs.bubblepaw-fixture /fictional/device",
        r#"/bin/sh -c "mkfs.bubblepaw-fixture /fictional/device""#,
    ] {
        let blocked = run(command).await?;
        assert!(blocked.is_error, "{command}");
        assert!(
            blocked.text_has("Blocked by hardline rule mkfs"),
            "{}",
            blocked.body()
        );
    }
    processes.shutdown().await;
    Ok(())
}
