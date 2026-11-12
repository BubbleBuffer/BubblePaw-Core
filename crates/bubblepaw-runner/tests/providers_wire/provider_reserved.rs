//! Transport regression: only the runtime submission name bypasses frozen declarations.
use axum::{Json, Router, extract::State, response::IntoResponse, routing::post};
use bubblepaw_core::{CancellationToken, Message, ModelRequest, Provider, Role, ToolDefinition};
use bubblepaw_runner::provider::{ChatCompletionsProvider, MessagesProvider, ResponsesProvider};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug)]
enum Adapter {
    Chat,
    Messages,
    Responses,
    Codex,
}
#[derive(Clone)]
struct Fixture {
    adapter: Adapter,
    streamed: bool,
    name: String,
    requests: Arc<Mutex<Vec<Value>>>,
}
async fn reply(State(state): State<Fixture>, Json(body): Json<Value>) -> axum::response::Response {
    state.requests.lock().unwrap().push(body);
    let name = &state.name;
    let arguments = json!({"value":17});
    let events = match state.adapter {
        Adapter::Chat => {
            let call = json!({"id":"call","type":"function","function":{"name":name,"arguments":arguments.to_string()}});
            if !state.streamed {
                return Json(json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[call]},"finish_reason":"tool_calls"}]})).into_response();
            }
            let (start, end) = name.split_at(name.len() / 2);
            vec![
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","type":"function","function":{"name":start,"arguments":""}}]}}]}),
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":end,"arguments":arguments.to_string()}}]},"finish_reason":"tool_calls"}]}),
            ]
        }
        Adapter::Messages => {
            let call = json!({"type":"tool_use","id":"call","name":name,"input":arguments});
            if !state.streamed {
                return Json(json!({"type":"message","role":"assistant","content":[call],"stop_reason":"tool_use"})).into_response();
            }
            vec![
                json!({"type":"message_start","message":{"id":"fixture","type":"message","role":"assistant","content":[]}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call","name":name,"input":{}}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":arguments.to_string()}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
                json!({"type":"message_stop"}),
            ]
        }
        Adapter::Responses | Adapter::Codex => vec![
            json!({"type":"response.completed","response":{"output":[{"type":"function_call","call_id":"call","name":name,"arguments":arguments.to_string()}]}}),
        ],
    };
    let mut body = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    if matches!(state.adapter, Adapter::Chat) {
        body.push_str("data: [DONE]\n\n");
    }
    ([("content-type", "text/event-stream")], body).into_response()
}
async fn fixture(
    adapter: Adapter,
    streamed: bool,
    name: &str,
) -> (Box<dyn Provider>, Fixture, tokio::task::JoinHandle<()>) {
    let state = Fixture {
        adapter,
        streamed,
        name: name.into(),
        requests: Arc::default(),
    };
    let app = Router::new()
        .route("/chat/completions", post(reply))
        .route("/messages", post(reply))
        .route("/responses", post(reply))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let auth_dir = tempfile::tempdir().unwrap();
    let auth = auth_dir.path().join("auth.json");
    std::fs::write(
        &auth,
        r#"{"tokens":{"access_token":"synthetic","account_id":"fixture"}}"#,
    )
    .unwrap();
    let provider: Box<dyn Provider> = match adapter {
        Adapter::Chat => Box::new(ChatCompletionsProvider::api_key("synthetic", base).unwrap()),
        Adapter::Messages => Box::new(MessagesProvider::api_key("synthetic", base).unwrap()),
        Adapter::Responses => Box::new(ResponsesProvider::api_key("synthetic", base).unwrap()),
        Adapter::Codex => Box::new(ResponsesProvider::codex_auth(&auth, base).unwrap()),
    };
    let task = tokio::spawn(async move {
        let _auth_dir = auth_dir;
        axum::serve(listener, app).await.unwrap();
    });
    (provider, state, task)
}
fn request(declaration: u8) -> ModelRequest {
    let mut request = ModelRequest {
        model: "fixture".into(),
        messages: vec![Message {
            id: "user".into(),
            session_id: "session".into(),
            role: Role::User,
            origin: None,
            content: "Submit the current result.".into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: String::new(),
        }],
        ..Default::default()
    };
    if declaration > 0 {
        request.tools.push(ToolDefinition {
            name:if declaration == 1 {"ordinary"} else {"submit_result"}.into(),
            description:"Frozen declaration".into(),
            input_schema:json!({"type":"object","properties":{"old":{"type":"string"}},"required":["old"],"additionalProperties":false}),
            read_only:true,approval:Default::default(),
        });
        if declaration == 3 {
            request.disabled_tools = std::mem::take(&mut request.tools);
        }
    }
    request
}
#[tokio::test]
async fn reserved_submission_reaches_runtime_without_or_with_stale_declarations() {
    for adapter in [
        Adapter::Chat,
        Adapter::Messages,
        Adapter::Responses,
        Adapter::Codex,
    ] {
        for streamed in [false, true] {
            for declaration in 0..=3 {
                let case = format!("{adapter:?} streamed={streamed} declaration={declaration}");
                let (provider, state, server) = fixture(adapter, streamed, "submit_result").await;
                let request = request(declaration);
                let frozen = request
                    .tools
                    .first()
                    .or_else(|| request.disabled_tools.first())
                    .cloned();
                let result = provider
                    .generate(request, Arc::new(|_| {}), CancellationToken::new())
                    .await
                    .unwrap_or_else(|error| panic!("{case}: {error}"));
                assert_eq!(result.tool_calls.len(), 1, "{case}");
                assert_eq!(result.tool_calls[0].name, "submit_result", "{case}");
                assert_eq!(
                    result.tool_calls[0].arguments,
                    json!({"value":17}),
                    "{case}"
                );
                let requests = state.requests.lock().unwrap();
                let body = &requests[0];
                if let Some(tool) = frozen {
                    let declaration = &body["tools"][0];
                    let schema = match adapter {
                        Adapter::Chat => &declaration["function"]["parameters"],
                        Adapter::Messages => &declaration["input_schema"],
                        _ => &declaration["parameters"],
                    };
                    assert_eq!(*schema, tool.input_schema);
                    assert_eq!(body["tools"].as_array().unwrap().len(), 1);
                } else {
                    assert!(body.get("tools").is_none());
                }
                assert!(body.get("response_format").is_none());
                assert!(body["output_config"].get("format").is_none());
                assert!(body.get("text").is_none());
                server.abort();
            }
        }
    }
}
#[tokio::test]
async fn other_undeclared_names_remain_rejected_in_every_transport() {
    for adapter in [
        Adapter::Chat,
        Adapter::Messages,
        Adapter::Responses,
        Adapter::Codex,
    ] {
        for streamed in [false, true] {
            let (provider, _, server) = fixture(adapter, streamed, "unknown").await;
            assert!(
                provider
                    .generate(request(1), Arc::new(|_| {}), CancellationToken::new())
                    .await
                    .is_err(),
                "{adapter:?}"
            );
            server.abort();
        }
    }
}
