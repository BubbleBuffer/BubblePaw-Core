use super::*;

struct ReplaceRequest;
#[async_trait]
impl ContextPreparation for ReplaceRequest {
    async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
        let mut request = call.request;
        request.system_prompt = "replacement".into();
        request.output_schema = None;
        request.tools = vec![Counter::default().definition()];
        let mut disabled = Counter::default().definition();
        disabled.name = "hook_disabled_leak".into();
        request.disabled_tools = vec![disabled];
        request.messages.clear();
        Ok(PreparedContext {
            request,
            notice: None,
            usage: None,
        })
    }
}

#[tokio::test]
async fn context_preparation_preserves_the_chat_prefix_and_does_not_restore_sent_budget()
-> Result<()> {
    let provider = Scripted::new(vec![reply("bad"), reply(r#"{"count":1}"#)]);
    let store = Store::in_memory()?;
    let session = store.create_session("replacement")?.id;
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            context: Arc::new(ReplaceRequest),
            system_prompt: "Persisted initial instructions".into(),
            ..Default::default()
        },
    );
    let options = RunOptions {
        max_tool_calls: Some(0),
        output_schema: Some(schema()),
        ..Default::default()
    };
    let expected = harness.effective_tool_definitions(&session, &options)?;
    let run = harness.prompt(&session, "work", options).await?;
    assert_eq!(run.status, "failed");
    assert!(
        run.error
            .unwrap()
            .contains("submit_result was not called after one host reminder")
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    // The completion tool remains reachable at zero budget. Neither active nor
    // disabled declarations supplied by the host replace the persisted prefix.
    for request in requests.iter() {
        assert!(request.output_schema.is_none());
        assert_eq!(
            serde_json::to_vec(&request.tools)?,
            serde_json::to_vec(&expected)?
        );
        assert!(request.disabled_tools.is_empty());
        assert_eq!(request.system_prompt, "Persisted initial instructions");
        assert!(
            !request
                .tools
                .iter()
                .any(|tool| tool.name == "count" || tool.name == "hook_disabled_leak")
        );
    }
    assert_eq!(budget_notices(&requests[0].messages).len(), 1);
    assert!(budget_notices(&requests[1].messages).is_empty());
    assert_eq!(budget_notices(&store.messages(&session)?).len(), 1);
    assert!(
        requests[1]
            .messages
            .last()
            .unwrap()
            .content
            .contains("Call submit_result")
    );
    Ok(())
}
