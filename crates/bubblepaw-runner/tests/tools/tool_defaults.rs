//! Scripted host-dispatch checks; no shell or live model is invoked.
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, Role, RunOptions, Store, Tool, ToolCall,
    ToolContext, ToolDefinition, ToolDispatcher, ToolRegistry, ToolResult,
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

fn registry(root: &Path) -> Result<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    WorkspaceTools::new(root)?.register(&mut registry, false)?;
    Ok(registry)
}

async fn dispatched(
    registry: &ToolRegistry,
    target: &'static str,
    input: Value,
) -> Result<ToolResult> {
    let mut tools = registry.clone();
    tools.register(Arc::new(Bridge(target)))?;
    let store = Store::in_memory()?;
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
    let harness = Harness::new(
        store.clone(),
        Arc::new(provider),
        tools,
        HarnessConfig::default(),
    );
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
        ToolResult::error(message.content)
    } else {
        ToolResult::text(message.content)
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
    let registry = registry(root.path())?;
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
        let result = dispatched(&registry, "read", input.clone()).await?;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, expected, "{input}");
    }
    // Over the result limit the text stops at a line boundary and the note names
    // the last returned line.
    let result = dispatched(&registry, "read", json!({"path":"wide.txt"})).await?;
    let (body, note) = result.content.rsplit_once('\n').context("missing note")?;
    let returned = body.lines().count();
    assert!(returned > 50 && returned < 200 && result.content.len() < 60 * 1024);
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
    let registry = registry(root.path())?;
    for (target, input) in [
        ("read", json!({"path":"missing/named-file.txt"})),
        ("list", json!({"path":"missing/named-directory"})),
        (
            "search",
            json!({"path":"missing/named-search-root","query":"needle"}),
        ),
    ] {
        let requested = input["path"].as_str().unwrap().to_owned();
        let result = dispatched(&registry, target, input).await?;
        assert!(result.is_error, "{target} unexpectedly succeeded");
        assert!(result.content.contains(&requested), "{}", result.content);
    }
    Ok(())
}

#[tokio::test]
async fn unknown_host_arguments_list_valid_names_before_invocation() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut registry = ToolRegistry::new();
    let processes = register_native_tools(&mut registry, root.path(), false)?;
    for (target, mut input) in [
        ("read", json!({"path":"missing.txt"})),
        ("list", json!({})),
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
        let result = dispatched(&registry, target, input).await?;
        assert!(result.is_error, "{target} unexpectedly succeeded");
        assert!(
            result
                .content
                .contains("unexpected arguments: unknownArgument"),
            "{}",
            result.content
        );
        assert!(
            result
                .content
                .contains(&format!("Valid arguments: {valid}.")),
            "{}",
            result.content
        );
        assert!(
            !result.content.contains("private-value"),
            "{}",
            result.content
        );
    }
    let result = dispatched(
        &registry,
        "poll",
        json!({"handle":"missing","cursor":{"typo":"private-value"}}),
    )
    .await?;
    assert!(result.is_error);
    assert!(
        result.content.contains("poll at /cursor"),
        "{}",
        result.content
    );
    assert!(
        result.content.contains("Valid arguments: stderr, stdout."),
        "{}",
        result.content
    );
    assert!(!result.content.contains("private-value"));
    processes.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn direct_workspace_invocation_names_valid_arguments() -> Result<()> {
    let root = tempfile::tempdir()?;
    let registry = registry(root.path())?;
    let error = registry
        .get_host("read")
        .unwrap()
        .invoke(
            ToolContext {
                session_id: "direct-test".into(),
                run_id: "direct-run".into(),
                invocation_id: "direct-call".into(),
                cancellation: CancellationToken::new(),
                approval: Default::default(),
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
