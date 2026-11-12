use super::*;

#[tokio::test]
async fn two_requests_in_one_run_keep_only_the_last_request_for_context_fill() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("submission after reminder")?;
    let provider = Arc::new(Responses(Mutex::new(std::collections::VecDeque::from([
        ModelResponse {
            text: "invalid JSON".into(),
            usage: Some(tokens(100)),
            ..Default::default()
        },
        ModelResponse {
            tool_calls: vec![bubblepaw_core::ToolCall {
                id: "answer-submitted".into(),
                name: "submit_result".into(),
                arguments: serde_json::json!({"answer":"yes"}),
            }],
            usage: Some(tokens(200)),
            ..Default::default()
        },
    ]))));
    let config = HarnessConfig {
        model: "local/model".into(),
        context: Arc::new(UsageContext::new(store.clone(), Arc::new(FullHistory))),
        ..Default::default()
    };
    let harness = Harness::new(store.clone(), provider, ToolRegistry::new(), config);
    let run = harness.prompt(&session.id, "answer", RunOptions {
        output_schema: Some(bubblepaw_core::JsonSchema::new("answer", serde_json::json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}))?),
        ..Default::default()
    }).await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    let report = session_usage(&store, &session.id, &UsageSettings::default())?;
    assert_eq!(report.context.last_request_input_tokens, Some(200));
    assert_eq!(report.own.tokens.input_tokens, 300);
    assert_eq!(report.own.recorded_entries, 1);
    Ok(())
}
