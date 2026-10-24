use bubblepaw_core::{CancellationToken, Message, ModelRequest, Provider, Role, ToolDefinition};
use bubblepaw_runner::provider::{ScriptedProvider, ScriptedStep};
use std::sync::Arc;

fn tool_message(call_id: &str, content: &str, is_error: bool) -> Message {
    Message {
        id: format!("result-{call_id}"),
        session_id: "resumed-session".into(),
        role: Role::Tool,
        content: content.into(),
        tool_calls: vec![],
        tool_call_id: Some(call_id.into()),
        origin: None,
        provider_state: None,
        is_error,
        created_at: String::new(),
    }
}

#[tokio::test]
async fn scripted_json_text_does_not_implicitly_submit_a_required_result() {
    let text = r#"{"answer":"ready"}"#;
    let provider =
        ScriptedProvider::with_steps(vec![ScriptedStep::Text { text: text.into() }], text);
    let request = ModelRequest {
        tools: vec![ToolDefinition {
            name: "submit_result".into(),
            description: "Submit required result".into(),
            input_schema: serde_json::json!({"type":"object"}),
            read_only: true,
            approval: Default::default(),
        }],
        ..Default::default()
    };
    for _ in 0..2 {
        let response = provider
            .generate(request.clone(), Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(response.text, text);
        assert!(response.tool_calls.is_empty());
    }
}

#[tokio::test]
async fn demo_validates_only_results_for_calls_it_emitted() {
    for tool_name in ["list", "js"] {
        let provider = ScriptedProvider::demo(false);
        let mut request = ModelRequest {
            model: "scripted".into(),
            system_prompt: String::new(),
            tools: vec![ToolDefinition {
                approval: Default::default(),
                name: tool_name.into(),
                description: String::new(),
                read_only: true,
                input_schema: serde_json::json!({"type":"object","properties":{"code":{"type":"string"}},"required":["code"]}),
            }],
            max_output_tokens: None,
            messages: vec![
                tool_message("old-unknown", "Previous effect outcome unknown", true),
                tool_message(
                    "old-process",
                    r#"{"pipe_only":true,"running":false,"exit_code":1}"#,
                    false,
                ),
            ],
            ..Default::default()
        };
        let first = provider
            .generate(request.clone(), Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(first.tool_calls.len(), 1);
        assert_eq!(first.tool_calls[0].name, tool_name);
        let call_id = &first.tool_calls[0].id;
        request
            .messages
            .push(tool_message(call_id, r#"{"entries":[]}"#, false));
        let final_response = provider
            .generate(request.clone(), Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap();
        assert!(final_response.tool_calls.is_empty());
        assert!(final_response.text.contains("completed"));
        request.messages.last_mut().unwrap().is_error = true;
        let error = provider
            .generate(request, Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("offline demo tool failed"));
    }
}
