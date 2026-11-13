//! Summary forks on the Messages and Responses wires: the fork body keeps the
//! main body's instructions, tools and prefix up to the cut, byte for byte.
use anyhow::Result;
use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
use bubblepaw_core::{
    CancellationToken, Message, ModelRequest, Provider, ReasoningEffort, Role, Store, ToolCall,
    ToolDefinition,
};
use bubblepaw_runner::{
    episode_models::{
        EpisodeFork, EpisodeModelConfig, EpisodeModels, HttpEpisodeModels, SummaryInput,
    },
    model_roles::ModelRoles,
    provider::{MessagesProvider, ProviderRegistry, ResponsesProvider},
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

const SUMMARY: &str = "# Recorded fix\nFailure remains [source-t2] [call-2].\nOpen: repair";
type Rows = Arc<Mutex<Vec<(HeaderMap, Value)>>>;

/// Serves the same event stream at `path` and records every request.
async fn serve(path: &'static str, events: Vec<Value>) -> Result<(String, Rows)> {
    let rows = Rows::default();
    let stream: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    let app = Router::new()
        .route(
            path,
            post(
                move |State(rows): State<Rows>, headers: HeaderMap, Json(body): Json<Value>| {
                    let stream = stream.clone();
                    async move {
                        rows.lock().unwrap().push((headers, body));
                        ([("content-type", "text/event-stream")], stream)
                    }
                },
            ),
        )
        .with_state(rows.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Ok((base, rows))
}

fn message(session: &str, id: &str, role: Role, content: &str) -> Message {
    Message {
        id: id.into(),
        session_id: session.into(),
        role,
        origin: None,
        content: content.into(),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: String::new(),
    }
}

/// A main request captured mid tool loop: user, two assistant steps carrying
/// the native state `state(step)` and their tool results.
fn request(
    session: &str,
    model: &str,
    state: impl Fn(usize) -> Value,
    adapter: &str,
    native_model: &str,
) -> ModelRequest {
    let step = |index: usize, text: &str| {
        let mut assistant = message(session, &format!("source-a{index}"), Role::Assistant, text);
        assistant.tool_calls.push(ToolCall {
            id: format!("call-{index}"),
            name: "js".into(),
            arguments: json!({}),
        });
        assistant.provider_state = Some(
            bubblepaw_core::with_provider_state_origin(
                state(index),
                adapter,
                Some(native_model),
                "native",
            )
            .unwrap(),
        );
        let mut tool = message(session, &format!("source-t{index}"), Role::Tool, "result");
        tool.tool_call_id = Some(format!("call-{index}"));
        [assistant, tool]
    };
    let mut messages = vec![message(session, "source-u", Role::User, "Fix the failure")];
    messages.extend(step(1, ""));
    messages.extend(step(2, "Checking"));
    ModelRequest {
        model: model.into(),
        system_prompt: "Frozen original opening instructions".into(),
        messages,
        tools: vec![ToolDefinition {
            name: "js".into(),
            description: "Frozen js declaration".into(),
            input_schema: json!({"type":"object"}),
            read_only: false,
            approval: Default::default(),
        }],
        max_output_tokens: Some(8000),
        reasoning_effort: Some(ReasoningEffort::High),
        session_id: session.into(),
        ..Default::default()
    }
}

/// Sends `request` as the main call, then summarizes it as a fork; returns the
/// recorded wire rows, main first.
async fn main_then_fork(
    provider: Arc<dyn Provider>,
    store: Store,
    request: ModelRequest,
    rows: Rows,
) -> Result<Vec<(HeaderMap, Value)>> {
    let session = request.session_id.clone();
    let registry = Arc::new(ProviderRegistry::new("p", provider));
    registry.set_usage_store(store.clone());
    // The main loop's last request, as the cache ledger records it.
    store
        .cache_accounting()
        .context
        .build(request.clone(), &session, "root", &[]);
    registry
        .generate(request.clone(), Arc::new(|_| {}), CancellationToken::new())
        .await?;
    let roles = ModelRoles::for_models(request.model.clone(), request.model.clone());
    let models = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles,
            retry_base_delay: Duration::ZERO,
        },
        registry.clone(),
        None,
    )?;
    models.record_fork_context(&session, &request, &[]);
    models
        .summarize_fork(
            SummaryInput {
                focus: None,
                session_id: session,
                previous_summary: None,
                turn_request: None,
                episode_text: "unused".into(),
                cap: 2000,
            },
            EpisodeFork {
                request,
                first_message_id: "source-u".into(),
                last_message_id: "source-t2".into(),
                provider: registry,
                max_request_bytes: 2 * 1024 * 1024,
                context_window: Some(200_000),
                fallback_window_tokens: 200_000,
                output_reserve_tokens: 4000,
                dispatched: true,
            },
        )
        .await?;
    let rows = rows.lock().unwrap().clone();
    assert_eq!(rows.len(), 2);
    Ok(rows)
}

fn without_marks(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| *key != "cache_control")
                .map(|(key, value)| (key.clone(), without_marks(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_marks).collect()),
        other => other.clone(),
    }
}

#[tokio::test]
async fn messages_fork_keeps_signed_thinking_prefix_and_marks_the_cut() -> Result<()> {
    let usage = json!({"input_tokens":501,"cache_read_input_tokens":400,"output_tokens":1});
    let (base, rows) = serve(
        "/v1/messages",
        vec![
            json!({"type":"message_start","message":{"usage":usage}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":SUMMARY}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":23}}),
            json!({"type":"message_stop"}),
        ],
    )
    .await?;
    let provider =
        MessagesProvider::api_key("synthetic-key", format!("{base}/v1"))?.with_opencode_route();
    let store = Store::in_memory()?;
    let session = store.create_session("Fork wire")?.id;
    let request = request(
        &session,
        "p/claude-test",
        |index| {
            let mut content = vec![
                json!({"type":"thinking","thinking":format!("thought {index}"),"signature":format!("SIG{index}")}),
            ];
            if index == 2 {
                content.push(json!({"type":"text","text":"Checking"}));
            }
            content.push(
                json!({"type":"tool_use","id":format!("call-{index}"),"name":"js","input":{}}),
            );
            json!({"adapter":"anthropic-messages","content":content})
        },
        "anthropic-messages",
        "claude-test",
    );
    let rows = main_then_fork(Arc::new(provider), store, request, rows).await?;
    let (main_headers, main) = &rows[0];
    let (fork_headers, fork) = &rows[1];
    for key in [
        "model",
        "system",
        "tools",
        "tool_choice",
        "thinking",
        "output_config",
        "max_tokens",
    ] {
        assert_eq!(fork[key], main[key], "{key}");
    }
    assert_eq!(
        fork_headers["x-opencode-session"],
        main_headers["x-opencode-session"]
    );
    let (main, fork) = (
        main["messages"].as_array().unwrap(),
        fork["messages"].as_array().unwrap(),
    );
    assert_eq!(fork.len(), main.len());
    assert_eq!(fork[1]["content"][0]["signature"], "SIG1");
    // Everything before the cut matches; the controls extend the last user turn.
    let last = main.len() - 1;
    assert_eq!(
        without_marks(&json!(fork[..last])),
        without_marks(&json!(main[..last]))
    );
    let blocks = fork[last]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 3);
    assert_eq!(
        without_marks(&blocks[0]),
        without_marks(&main[last]["content"][0])
    );
    assert_eq!(blocks[0]["cache_control"], json!({"type":"ephemeral"}));
    let control = blocks[1]["text"].as_str().unwrap();
    assert!(control.starts_with("<harness>\nEPISODE MEMORY BRANCH"));
    assert!(control.contains(r#""wire_positions":["1.0","1.1"]"#));
    assert!(control.contains(r#""wire_positions":["3.0","3.1","3.2"]"#));
    assert!(control.contains(r#""wire_positions":["4.0"]"#));
    assert!(
        blocks[2]["text"]
            .as_str()
            .unwrap()
            .starts_with("HOST-GENERATED SUMMARY RETURN CONTROL")
    );
    Ok(())
}

#[tokio::test]
async fn responses_fork_keeps_encrypted_reasoning_prefix_and_instructions() -> Result<()> {
    let output = json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":SUMMARY}]}]);
    let (base, rows) = serve(
        "/responses",
        vec![
            json!({"type":"response.output_text.delta","delta":SUMMARY}),
            json!({"type":"response.completed","response":{"output":output,"usage":{"input_tokens":501,"input_tokens_details":{"cached_tokens":400},"output_tokens":23}}}),
        ],
    )
    .await?;
    let provider = ResponsesProvider::api_key("synthetic-key", base)?.with_opencode_route();
    let store = Store::in_memory()?;
    let session = store.create_session("Fork wire")?.id;
    let request = request(
        &session,
        "p/gpt-test",
        |index| {
            let mut items = vec![
                json!({"type":"reasoning","id":format!("rs_{index}"),"summary":[],"encrypted_content":format!("ENC{index}")}),
            ];
            if index == 2 {
                items.push(json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Checking"}]}));
            }
            items.push(
                json!({"type":"function_call","call_id":format!("call-{index}"),"name":"js","arguments":"{}"}),
            );
            json!({"adapter":"responses","items":items})
        },
        "responses",
        "gpt-test",
    );
    let rows = main_then_fork(Arc::new(provider), store, request, rows).await?;
    let (main_headers, main) = &rows[0];
    let (fork_headers, fork) = &rows[1];
    for key in [
        "model",
        "instructions",
        "tools",
        "reasoning",
        "include",
        "store",
        "max_output_tokens",
    ] {
        assert_eq!(fork[key], main[key], "{key}");
    }
    assert_eq!(
        fork_headers["x-opencode-session"],
        main_headers["x-opencode-session"]
    );
    let (main, fork) = (
        main["input"].as_array().unwrap(),
        fork["input"].as_array().unwrap(),
    );
    assert_eq!(&fork[..main.len()], main);
    assert_eq!(fork.len(), main.len() + 2);
    assert_eq!(fork[4]["encrypted_content"], "ENC2");
    assert_eq!(fork[main.len()]["role"], "user");
    let control = fork[main.len()]["content"][0]["text"].as_str().unwrap();
    assert!(control.starts_with("<harness>\nEPISODE MEMORY BRANCH"));
    assert!(control.contains(r#""wire_positions":[1,2]"#));
    assert!(control.contains(r#""wire_positions":[4,5,6]"#));
    assert!(control.contains(r#""wire_positions":[7]"#));
    assert_eq!(fork[main.len() + 1]["role"], "user");
    Ok(())
}

/// The Codex subscription backend caches per session id: the main request and
/// its summary fork must send the same key in the body and the headers.
#[tokio::test]
async fn codex_fork_shares_the_prompt_cache_key_with_its_main_request() -> Result<()> {
    let output = json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":SUMMARY}]}]);
    let (base, rows) = serve(
        "/responses",
        vec![
            json!({"type":"response.output_text.delta","delta":SUMMARY}),
            json!({"type":"response.completed","response":{"output":output,"usage":{"input_tokens":501,"input_tokens_details":{"cached_tokens":400},"output_tokens":23}}}),
        ],
    )
    .await?;
    let directory = tempfile::tempdir()?;
    let auth = directory.path().join("auth.json");
    std::fs::write(
        &auth,
        json!({"tokens":{"access_token":"synthetic-access","account_id":"synthetic-account"}})
            .to_string(),
    )?;
    let provider = ResponsesProvider::codex_auth(&auth, base)?;
    let store = Store::in_memory()?;
    let session = store.create_session("Fork wire")?.id;
    let request = request(
        &session,
        "p/gpt-test",
        |index| {
            json!({"adapter":"responses","items":[
                {"type":"reasoning","id":format!("rs_{index}"),"summary":[],"encrypted_content":format!("ENC{index}")},
                {"type":"function_call","call_id":format!("call-{index}"),"name":"js","arguments":"{}"}
            ]})
        },
        "responses",
        "gpt-test",
    );
    let rows = main_then_fork(Arc::new(provider), store, request, rows).await?;
    let (main_headers, main) = &rows[0];
    let (fork_headers, fork) = &rows[1];
    assert_eq!(main["prompt_cache_key"], json!(session));
    assert_eq!(fork["prompt_cache_key"], main["prompt_cache_key"]);
    for name in ["session-id", "thread-id"] {
        assert_eq!(main_headers[name].to_str()?, session);
        assert_eq!(fork_headers[name], main_headers[name]);
    }
    Ok(())
}
