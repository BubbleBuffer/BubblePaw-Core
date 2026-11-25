use super::*;

#[tokio::test]
async fn default_round_limit_allows_more_than_thirty_two_requests() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("long root run")?;
    let mut responses = (0..40)
        .map(|round| ModelResponse {
            tool_calls: vec![call(&format!("round-{round}"))],
            ..Default::default()
        })
        .collect::<Vec<_>>();
    responses.push(answer("finished naturally"));
    let provider = Arc::new(Script::new(responses));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Native {
        contexts: Mutex::default(),
        result: "ok".into(),
    }))?;
    let agent = harness(store.clone(), provider.clone(), tools);
    assert_eq!(agent.max_rounds(), None);
    let run = agent
        .prompt(&session.id, "start", RunOptions::default())
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(run.text.as_deref(), Some("finished naturally"));
    assert_eq!(provider.requests.lock().unwrap().len(), 41);
    assert_eq!(
        store
            .messages(&session.id)?
            .iter()
            .filter(|m| m.role == Role::Tool)
            .count(),
        40
    );
    Ok(())
}
