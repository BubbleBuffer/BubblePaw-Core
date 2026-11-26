//! Exercise workspace mutation through its checkpointed Harness dispatcher contract.
use anyhow::{Context, Result, bail, ensure};
use bubblepaw_core::{Harness, HarnessConfig, Role, RunOptions, Store, ToolCall, ToolRegistry};
use bubblepaw_runner::provider::{ScriptedProvider, ScriptedStep};
use bubblepaw_runner::tools::{ProcessTools, WorkspaceTools};
use serde_json::Value;
use std::sync::Arc;

/// The one store these helpers and the registries under test share: the
/// native tools hold their store from registration, so a registry and the
/// harness that runs it must use the same one.
pub fn store() -> Store {
    static STORE: std::sync::OnceLock<Store> = std::sync::OnceLock::new();
    STORE
        .get_or_init(|| Store::in_memory().expect("in-memory store"))
        .clone()
}

/// The native pack over the shared store, with processes that run for sessions
/// the store does not know; the fixtures here call `exec` with made-up owners.
pub fn native_pack(
    registry: &mut ToolRegistry,
    root: &std::path::Path,
    read_only: bool,
) -> Result<Arc<ProcessTools>> {
    registry.show_always(bubblepaw_runner::tools::ALWAYS_SHOWN_TOOLS.iter().copied());
    let workspace = WorkspaceTools::new(root)?;
    workspace.register(registry, read_only, &store())?;
    let processes = Arc::new(ProcessTools::new(workspace));
    processes.register_wait(registry)?;
    if !read_only {
        processes.register_without_store(registry)?;
    }
    Ok(processes)
}

pub async fn invoke(registry: &ToolRegistry, name: &str, input: Value) -> Result<Value> {
    let store = store();
    let session = store.create_session("workspace mutation contract")?;
    let root = tempfile::tempdir()?;
    let artifacts = Arc::new(bubblepaw_runner::artifacts::ArtifactStore::new(
        root.path(),
        root.path().join("artifacts"),
    )?);
    let mut tools = registry.clone();
    Arc::new(bubblepaw_runner::codemode::CodeMode::new(
        root.path().to_owned(),
        artifacts,
    )?)
    .register(&mut tools)?;
    let provider = ScriptedProvider::with_steps(
        vec![ScriptedStep::Calls {
            calls: vec![ToolCall {
                id: "mutation-call".into(),
                name: "js".into(),
                arguments: serde_json::json!({"code":format!("return await tools.{name}({input});")}),
            }],
        }],
        "done",
    );
    let harness = Harness::new(
        store.clone(),
        Arc::new(provider),
        tools,
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    );
    let run = harness
        .prompt(
            &session.id,
            "perform the fixture write",
            RunOptions::default(),
        )
        .await?;
    ensure!(
        run.status == "completed",
        "mutation run failed: {:?}",
        run.error
    );
    let result = store
        .messages(&session.id)?
        .into_iter()
        .find(|message| {
            message.role == Role::Tool && message.tool_call_id.as_deref() == Some("mutation-call")
        })
        .context("mutation tool result is missing")?;
    if result.is_error {
        bail!("{}", result.text());
    }
    ensure!(
        !store
            .checkpoint_records_page(&session.id, None, 1)?
            .is_empty(),
        "successful mutation did not persist a checkpoint"
    );
    Ok(serde_json::from_str(
        result.text().split("\n[---]\n").next().unwrap(),
    )?)
}
