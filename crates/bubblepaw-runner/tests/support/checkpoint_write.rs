//! Exercise workspace mutation through its checkpointed Harness dispatcher contract.
use anyhow::{Context, Result, bail, ensure};
use bubblepaw_core::{Harness, HarnessConfig, Role, RunOptions, Store, ToolCall, ToolRegistry};
use bubblepaw_runner::provider::{ScriptedProvider, ScriptedStep};
use serde_json::Value;
use std::sync::Arc;

pub async fn invoke(registry: &ToolRegistry, name: &str, input: Value) -> Result<Value> {
    let store = Store::in_memory()?;
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
        bail!(result.content);
    }
    ensure!(
        !store
            .checkpoint_records_page(&session.id, None, 1)?
            .is_empty(),
        "successful mutation did not persist a checkpoint"
    );
    Ok(serde_json::from_str(
        result.content.split("\n[---]\n").next().unwrap(),
    )?)
}
