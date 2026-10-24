use super::*;

#[tokio::test]
#[ignore = "Makes one paid live structured-output model request; opt in explicitly"]
async fn live_schema_smoke() {
    let Some(provider) = live_provider() else {
        return;
    };
    #[derive(Debug, serde::Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Answer {
        status: String,
        count: u32,
    }
    let response = provider.generate(
        ModelRequest {
            model: "claude-haiku-5-5".into(),
            session_id: uuid::Uuid::new_v4().to_string(),
            system_prompt: "Call submit_result with status ready and count 2.".into(),
            messages: vec![fixture_message(Role::User, "Run the structured output check.")],
            reasoning_effort: Some(ReasoningEffort::Low),
            tools: vec![ToolDefinition {
                name: "submit_result".into(),
                description: "Submit the completed result.".into(),
                input_schema: json!({
                "type":"object","additionalProperties":false,
                "properties":{"status":{"type":"string","enum":["ready"]},"count":{"type":"integer","enum":[2]}},
                "required":["status","count"]
            }),
                read_only: true,
                approval: Default::default(),
            }],
            max_output_tokens: Some(1024),
            ..Default::default()
        }, Arc::new(|_| {}), CancellationToken::new()
    ).await.unwrap();
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].name, "submit_result");
    assert_eq!(
        serde_json::from_value::<Answer>(response.tool_calls[0].arguments.clone()).unwrap(),
        Answer {
            status: "ready".into(),
            count: 2
        }
    );
    println!("Structured output smoke passed.");
}
