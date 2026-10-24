use super::*;

struct PendingTool {
    entered: Arc<Notify>,
}
#[async_trait]
impl Tool for PendingTool {
    fn definition(&self) -> ToolDefinition {
        definition()
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        self.entered.notify_one();
        std::future::pending().await
    }
}
#[tokio::test]
async fn pending_tool_cancellation_records_unknown_and_preserves_followup() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("archive.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("tool stop")?;
    let entered = Arc::new(Notify::new());
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(PendingTool {
        entered: entered.clone(),
    }))?;
    let provider = Arc::new(Script::new(vec![ModelResponse {
        tool_calls: vec![call("pending")],
        ..Default::default()
    }]));
    let agent = harness(store.clone(), provider, tools);
    agent.start(&session.id, "begin", RunOptions::default())?;
    entered.notified().await;
    agent.followup(&session.id, "later")?;
    agent.shutdown(Duration::from_secs(1)).await?;
    assert_eq!(agent.wait_for_idle(&session.id).await?.status, "cancelled");
    let messages = store.messages(&session.id)?;
    assert!(messages[2].content.contains("outcome unknown"));
    drop(agent);
    drop(store);
    let reopened = Store::open(path)?;
    let provider = Arc::new(Script::new(vec![answer("resumed")]));
    let agent = harness(reopened.clone(), provider.clone(), ToolRegistry::new());
    let resumed = agent.resume(&session.id, RunOptions::default()).await?;
    assert_eq!(resumed.status, "completed");
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests[0].messages.len(), 5);
    assert_eq!(requests[0].messages[2].role, Role::Tool);
    assert_eq!(requests[0].messages[3].content, "later");
    assert!(requests[0].messages[4].content.contains("tools_reloaded"));
    assert!(requests[0].messages[4].content.contains("native"));
    Ok(())
}
