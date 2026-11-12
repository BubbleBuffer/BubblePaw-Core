use super::*;

#[tokio::test]
async fn every_provider_request_has_a_transient_boundary_before_callback_text() -> Result<()> {
    let mut result = submit(json!({"count":1}));
    result.text = r#"{"count":1}"#.into();
    let provider = Scripted::new(vec![reply("bad"), result]);
    let store = Store::in_memory()?;
    let session = store.create_session("request boundaries")?.id;
    let harness = Harness::new(
        store,
        Arc::new(Streamed(provider)),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    let mut events = harness.subscribe();
    let run = harness
        .prompt(
            &session,
            "give JSON",
            RunOptions {
                output_schema: Some(schema()),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    let mut timeline = vec![];
    while let Ok(event) = events.try_recv() {
        match event.kind {
            EventKind::ModelRequestStarted {} => {
                assert!(!event.durable);
                timeline.push("request".to_string());
            }
            EventKind::TextDelta { text } => timeline.push(text),
            _ => {}
        }
    }
    assert_eq!(
        timeline,
        vec!["request", "bad", "request", r#"{"count":1}"#]
    );
    Ok(())
}

#[tokio::test]
async fn schema_becomes_result_tool_and_missing_submission_gets_one_nudge_at_round_limit()
-> Result<()> {
    let provider = Scripted::new(vec![reply(r#"{"count":0}"#), submit(json!({"count":2}))]);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Counter::default()))?;
    let (harness, store, session) = harness(provider.clone(), tools)?;
    let run = harness
        .prompt(
            &session,
            "give JSON",
            RunOptions {
                output_schema: Some(schema()),
                max_rounds: Some(1),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(run.text.as_deref(), Some(r#"{"count":2}"#));
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.output_schema.is_none())
    );
    assert!(requests[0].disabled_tools.is_empty());
    assert!(requests[1].disabled_tools.is_empty());
    assert_eq!(
        serde_json::to_value(&requests[1].tools)?,
        serde_json::to_value(&requests[0].tools)?
    );
    assert_eq!(requests[0].system_prompt, requests[1].system_prompt);
    assert!(
        requests[1]
            .messages
            .last()
            .unwrap()
            .content
            .contains("Call submit_result")
    );
    assert!(
        store
            .messages(&session)?
            .iter()
            .any(|message| message.content == r#"{"count":0}"#)
    );
    Ok(())
}

#[tokio::test]
async fn two_text_only_replies_fail_after_exactly_one_nudge() -> Result<()> {
    let provider = Scripted::new(vec![
        reply("not JSON"),
        reply(r#"{"count":"two"}"#),
        reply(r#"{"count":2}"#),
    ]);
    let (harness, store, session) = harness(provider.clone(), ToolRegistry::new())?;
    let run = harness
        .prompt(
            &session,
            "give JSON",
            RunOptions {
                output_schema: Some(schema()),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(run.status, "failed");
    assert!(
        run.error
            .unwrap()
            .contains("submit_result was not called after one host reminder")
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .messages
            .last()
            .unwrap()
            .content
            .contains("Call submit_result")
    );
    assert!(
        store
            .messages(&session)?
            .iter()
            .any(|message| message.content == r#"{"count":"two"}"#)
    );
    Ok(())
}

#[tokio::test]
async fn invalid_schema_is_rejected_before_accepting_input() -> Result<()> {
    let provider = Scripted::new(vec![]);
    let (harness, store, session) = harness(provider.clone(), ToolRegistry::new())?;
    let error = harness
        .start(
            &session,
            "never admitted",
            RunOptions {
                output_schema: Some(JsonSchema {
                    name: "answer".into(),
                    schema: json!({"type":"impossible"}),
                }),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("Invalid run output schema"));
    assert!(store.messages(&session)?.is_empty());
    assert!(provider.requests.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn model_less_script_cannot_silently_bypass_output_schema_validation() -> Result<()> {
    let provider = Scripted::new(vec![]);
    let (harness, store, session) = harness(provider, ToolRegistry::new())?;
    let error = harness
        .start(
            &session,
            "never admitted",
            RunOptions {
                output_schema: Some(schema()),
                script: Some("return {}".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("cannot be combined with script"));
    assert!(store.messages(&session)?.is_empty());
    Ok(())
}
