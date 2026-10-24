#[path = "support/http_server.rs"]
mod http_server;
use axum::{Json, Router, response::IntoResponse, routing::post};
use bubblepaw_core::{
    CancellationToken, ModelRequest, Provider, ProviderError, ProviderErrorKind, ToolDefinition,
};
use bubblepaw_runner::provider::ResponsesProvider;
use http_server::serve;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn request() -> ModelRequest {
    ModelRequest {
        model: "mock-model".into(),
        system_prompt: "test".into(),
        messages: Vec::new(),
        tools: vec![ToolDefinition {
            approval: Default::default(),
            name: "workspace_read".into(),
            description: "read".into(),
            read_only: true,
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"},"start_line":{"type":"integer"}},"required":["path"],"additionalProperties":false}),
        }],
        max_output_tokens: Some(100),
        ..Default::default()
    }
}
fn sse() -> String {
    let call = json!({"type":"function_call","call_id":"call-1","name":"workspace_read","arguments":"{\"path\":\"README.md\"}"});
    [json!({"type":"response.output_text.delta","delta":"Reading."}),json!({"type":"response.output_item.done","output_index":0,"item":call}),json!({"type":"response.completed","response":{"output":[{"type":"reasoning","encrypted_content":"synthetic-encrypted"},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Reading."}]},call],"usage":{"input_tokens":12,"output_tokens":4}}})].iter().map(|event|format!("data: {event}\r\n\r\n")).collect()
}

#[tokio::test]
async fn tools_off_round_keeps_declarations_and_rejects_any_call() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let sink = observed.clone();
    let done = format!(
        "data: {}\n\n",
        json!({"type":"response.completed","response":{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done."}]}]}})
    );
    let app = Router::new().route(
        "/responses",
        post(move |Json(body): Json<Value>| {
            let sink = sink.clone();
            let done = done.clone();
            async move {
                // The model still returns a call on a tools-off round; the host must reject it.
                let off = body["tool_choice"] == "none";
                sink.lock().unwrap().push(body);
                let events = if off { sse() } else { done };
                ([("content-type", "text/event-stream")], events).into_response()
            }
        }),
    );
    let (base, server) = serve(app).await;
    let provider = ResponsesProvider::api_key("synthetic-key", base).unwrap();
    let result = provider
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.text, "Done.");
    let mut tools_off = request();
    tools_off.disabled_tools = std::mem::take(&mut tools_off.tools);
    let error = provider
        .generate(tools_off, Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        ProviderError::of(&error).unwrap().kind,
        ProviderErrorKind::Fatal
    );
    let observed = observed.lock().unwrap();
    assert!(observed[0].get("tool_choice").is_none());
    assert_eq!(observed[1]["tools"], observed[0]["tools"]);
    assert_eq!(observed[1]["tool_choice"], "none");
    server.abort();
}
