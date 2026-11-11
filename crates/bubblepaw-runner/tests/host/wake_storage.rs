#![cfg(unix)]
//! Bound canonical writes while an exec spool and a wake response stream grow.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, Store,
    TextCallback, ToolContext, ToolRegistry,
};
use bubblepaw_runner::{
    config::ExecConfig,
    tools::{ProcessTools, WorkspaceTools},
};
use serde_json::json;
use std::{sync::Arc, time::Duration};

struct Fragments;
#[async_trait]
impl Provider for Fragments {
    async fn generate(
        &self,
        _: ModelRequest,
        on_text: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        for _ in 0..1000 {
            on_text("stream ");
        }
        Ok(ModelResponse {
            text: "stream ".repeat(1000),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn completion_wake_keeps_exec_output_and_stream_fragments_out_of_sqlite() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path().join("data"))?;
    let parent = store.create_session("storage fixture")?;
    let processes = Arc::new(ProcessTools::with_config(
        WorkspaceTools::new(directory.path())?,
        ExecConfig::default(),
        directory.path().join("data"),
    )?);
    processes.set_store(store.clone());
    let mut tools = ToolRegistry::new();
    processes.register(&mut tools)?;
    let exec = tools.get("exec").unwrap();
    let harness = Arc::new(Harness::new(
        store.clone(),
        Arc::new(Fragments),
        tools,
        HarnessConfig::default(),
    ));
    let wake = processes.wake_coordinator();
    wake.configure(true, Duration::from_millis(20));
    let _registration = wake.register_harness(harness.clone());
    let _driver = wake.start_driver();
    let before = store.storage_stats()?;
    exec.invoke(
        ToolContext {
            session_id: parent.id.clone(),
            run_id: "synthetic-launch".into(),
            invocation_id: "synthetic-invocation".into(),
            cancellation: CancellationToken::new(),
            approval: Default::default(),
        },
        json!({"command":"head -c 2097152 /dev/zero", "background":true}),
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while store
            .runs(&parent.id)?
            .last()
            .is_none_or(|run| run.status != "completed")
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let after = store.storage_stats()?;
    let writes = after.transaction_count - before.transaction_count;
    let wal_growth = after.wal_bytes.saturating_sub(before.wal_bytes);
    println!(
        "wake storage: 2097152 exec bytes, 1000 model fragments; {writes} transactions, {wal_growth} WAL bytes, {} database bytes, {} messages",
        after.database_bytes, after.message_count
    );
    assert_eq!(after.message_count - before.message_count, 2);
    assert!(writes <= 8, "unbounded wake writes: {writes}");
    assert!(wal_growth < 512 * 1024, "unbounded wake WAL: {wal_growth}");
    assert!(after.database_bytes < 512 * 1024);
    let messages = store.messages(&parent.id)?;
    assert!(messages[0].content.contains("stdout 2097152 bytes"));
    assert_eq!(messages[1].content, "stream ".repeat(1000));
    processes.shutdown().await;
    harness.shutdown(Duration::from_secs(2)).await?;
    Ok(())
}
