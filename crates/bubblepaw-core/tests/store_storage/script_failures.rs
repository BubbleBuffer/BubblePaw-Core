//! Script failures preserve canonical blob ownership across restart.
use super::*;

struct FailingScript(String);
#[async_trait::async_trait]
impl bubblepaw_core::Tool for FailingScript {
    fn definition(&self) -> bubblepaw_core::ToolDefinition {
        bubblepaw_core::ToolDefinition {
            name: "js".into(),
            description: "Scripted failure fixture".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Default::default(),
        }
    }
    async fn invoke(
        &self,
        _: bubblepaw_core::ToolContext,
        _: serde_json::Value,
    ) -> Result<bubblepaw_core::ToolResult> {
        Ok(bubblepaw_core::ToolResult::error(&self.0))
    }
}

#[tokio::test]
async fn large_script_failure_reuses_tool_blob_and_restores_run_errors_after_restart() -> Result<()>
{
    let error = "script failure with repeated diagnostic detail\n".repeat(3000);
    script_failure_case(error[..100 * 1024].to_owned()).await
}

#[tokio::test]
async fn small_script_failure_reuses_tool_blob_and_restores_run_errors_after_restart() -> Result<()>
{
    script_failure_case("Small script failure with 猫 and\nnewlines.".into()).await
}

async fn script_failure_case(error: String) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("script-error.db");
    let store = Store::open(&path)?;
    let session = store.create_session("script failure")?;
    let mut tools = bubblepaw_core::ToolRegistry::new();
    tools.register(std::sync::Arc::new(FailingScript(error.clone())))?;
    let agent = bubblepaw_core::Harness::new(
        store.clone(),
        std::sync::Arc::new(InputProvider),
        tools,
        bubblepaw_core::HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            max_tool_result_bytes: 128 * 1024,
            ..Default::default()
        },
    );
    let run = agent
        .resume(
            &session.id,
            bubblepaw_core::RunOptions {
                script: Some("throw fixture".into()),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(run.status, "failed");
    assert_eq!(run.error.as_deref(), Some(error.as_str()));
    assert_eq!(store.run(&run.id)?.error.as_deref(), Some(error.as_str()));
    assert!(store.run(&run.id)?.text.is_none());
    assert_eq!(
        store.runs(&session.id)?[0].error.as_deref(),
        Some(error.as_str())
    );
    assert_eq!(
        store.snapshot(&session.id, 10, 10, 10)?.runs[0]
            .error
            .as_deref(),
        Some(error.as_str())
    );
    assert_eq!(
        store.session_summaries(10)?[0]
            .latest_run
            .as_ref()
            .unwrap()
            .error
            .as_deref(),
        Some(error.as_str())
    );
    let conn = Connection::open(store.chat_path(&session.id)?.unwrap())?;
    let tool_data: String = conn.query_row(
        "SELECT data FROM messages WHERE json_extract(data,'$.role')='tool'",
        [],
        |row| row.get(0),
    )?;
    store.flush_serving()?;
    let serving = Connection::open(Store::legacy_data_dir(&path).join("serving.db"))?;
    let run_data: String =
        serving.query_row("SELECT data FROM runs WHERE id=?1", [&run.id], |row| {
            row.get(0)
        })?;
    let tool_data: serde_json::Value = serde_json::from_str(&tool_data)?;
    let run_data: serde_json::Value = serde_json::from_str(&run_data)?;
    assert!(run_data["error"].is_null());
    assert_eq!(run_data["_error_blob"], tool_data["_content_blob"]);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get::<_, i64>(0))?,
        1
    );
    assert!(agent.events_after(0, Some(&session.id), 100)?.iter().any(|event| {
        matches!(&event.kind, bubblepaw_core::EventKind::RunFinished { error: Some(text), .. } if text == &error)
    }));
    drop(conn);
    drop(agent);
    drop(store);
    let reopened = Store::open(&path)?;
    assert_eq!(
        reopened.run(&run.id)?.error.as_deref(),
        Some(error.as_str())
    );
    assert!(input_harness(&reopened).events_after(0, Some(&session.id), 100)?.iter().any(|event| {
        matches!(&event.kind, bubblepaw_core::EventKind::RunFinished { error: Some(text), .. } if text == &error)
    }));
    Ok(())
}
