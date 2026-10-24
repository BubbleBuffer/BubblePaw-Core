use axum::{Json, Router, extract::State, response::IntoResponse, routing::post};
use bubblepaw_core::{
    CancellationToken, JsonSchema, Message, ModelRequest, Provider, ReasoningEffort, Role,
    ToolDefinition,
};
use bubblepaw_runner::provider::{ChatCompletionsProvider, MessagesProvider, ResponsesProvider};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
enum Adapter {
    Messages,
    Responses,
    Chat,
    Codex,
}

#[derive(Clone)]
struct Fixture {
    adapter: Adapter,
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn reply(
    State(fixture): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    fixture.requests.lock().unwrap().push(body);
    let text = r#"{"title":"ready","count":2}"#;
    match fixture.adapter {
        Adapter::Messages => Json(json!({"type":"message","role":"assistant","content":[{"type":"text","text":text}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":3}})).into_response(),
        Adapter::Responses | Adapter::Codex => {
            let event = json!({"type":"response.completed","response":{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}],"usage":{"input_tokens":2,"output_tokens":3}}});
            ([("content-type", "text/event-stream")], format!("data: {event}\n\n")).into_response()
        }
        Adapter::Chat => Json(json!({"choices":[{"message":{"role":"assistant","content":text}}],"usage":{"prompt_tokens":2,"completion_tokens":3}})).into_response(),
    }
}

async fn fixture(adapter: Adapter) -> (Box<dyn Provider>, Fixture, tokio::task::JoinHandle<()>) {
    let fixture = Fixture {
        adapter,
        requests: Arc::default(),
    };
    let path = match adapter {
        Adapter::Messages => "/messages",
        Adapter::Responses | Adapter::Codex => "/responses",
        Adapter::Chat => "/chat/completions",
    };
    let app = Router::new()
        .route(path, post(reply))
        .with_state(fixture.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let auth_dir = tempfile::tempdir().unwrap();
    let auth = auth_dir.path().join("auth.json");
    std::fs::write(
        &auth,
        r#"{"tokens":{"access_token":"synthetic","account_id":"synthetic-account"}}"#,
    )
    .unwrap();
    let provider: Box<dyn Provider> = match adapter {
        Adapter::Messages => Box::new(MessagesProvider::api_key("synthetic", base).unwrap()),
        Adapter::Responses => Box::new(ResponsesProvider::api_key("synthetic", base).unwrap()),
        Adapter::Codex => Box::new(ResponsesProvider::codex_auth(&auth, base).unwrap()),
        Adapter::Chat => Box::new(ChatCompletionsProvider::api_key("synthetic", base).unwrap()),
    };
    let task = tokio::spawn(async move {
        let _auth_dir = auth_dir;
        axum::serve(listener, app).await.unwrap()
    });
    (provider, fixture, task)
}

fn request() -> ModelRequest {
    ModelRequest {
        model: "fixture".into(),
        messages: vec![Message {
            id: "user".into(),
            session_id: "fixture".into(),
            role: Role::User,
            origin: None,
            content: "Return the answer.".into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: String::new(),
        }],
        tools: vec![ToolDefinition {
            name: "submit_result".into(),
            description: "Submit the completed result.".into(),
            input_schema: json!({"type":"object","properties":{"title":{"type":"string"},"count":{"type":"integer"}},"required":["title","count"],"additionalProperties":false}),
            read_only: true,
            approval: Default::default(),
        }],
        output_schema: Some(
            JsonSchema::new(
                "answer",
                json!({
                    "type":"object","additionalProperties":false,
                    "properties":{"title":{"type":"string"},"count":{"type":"integer"}},
                    "required":["title","count"]
                }),
            )
            .unwrap(),
        ),
        ..Default::default()
    }
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Answer {
    title: String,
    count: u32,
}

#[tokio::test]
async fn legacy_output_schema_never_reaches_provider_wire_and_tool_schema_is_unchanged() {
    for adapter in [
        Adapter::Messages,
        Adapter::Responses,
        Adapter::Chat,
        Adapter::Codex,
    ] {
        let (provider, fixture, server) = fixture(adapter).await;
        for effort in [None, Some(ReasoningEffort::High)] {
            let mut request = request();
            request.reasoning_effort = effort;
            let tool_schema = request.tools[0].input_schema.clone();
            let result = provider
                .generate(request, Arc::new(|_| {}), CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(
                result.parse_json::<Answer>().unwrap(),
                Answer {
                    title: "ready".into(),
                    count: 2
                }
            );
            let body = fixture.requests.lock().unwrap().last().unwrap().clone();
            assert_no_response_schema(&body);
            let expected_effort = effort.unwrap_or_default().as_str();
            match adapter {
                Adapter::Messages => {
                    assert_eq!(body["output_config"], json!({"effort":expected_effort}));
                    assert_eq!(body["thinking"], json!({"type":"adaptive"}));
                    assert_eq!(body["tools"][0]["input_schema"], tool_schema);
                }
                Adapter::Responses | Adapter::Codex => {
                    assert_eq!(body["reasoning"]["effort"], expected_effort);
                    assert_eq!(body["tools"][0]["parameters"], tool_schema);
                }
                Adapter::Chat => {
                    assert_eq!(body["reasoning_effort"], expected_effort);
                    assert_eq!(body["tools"][0]["function"]["parameters"], tool_schema);
                }
            }
        }
        server.abort();
    }
}

fn assert_no_response_schema(body: &Value) {
    assert!(body.get("output_schema").is_none());
    assert!(body.get("response_format").is_none());
    assert!(body["output_config"].get("format").is_none());
    assert!(body.get("text").is_none());
}

#[tokio::test]
async fn unused_invalid_legacy_schema_does_not_change_transport() {
    for adapter in [
        Adapter::Messages,
        Adapter::Responses,
        Adapter::Chat,
        Adapter::Codex,
    ] {
        let (provider, fixture, server) = fixture(adapter).await;
        let mut request = request();
        request.output_schema.as_mut().unwrap().schema =
            json!({"type":"object","properties":{"child":{"type":"object"}}});
        provider
            .generate(request, Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap();
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_no_response_schema(&requests[0]);
        server.abort();
    }
}
