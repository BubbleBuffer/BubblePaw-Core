//! Replies that a length stop cuts are continued and joined, for each HTTP API.
//! The mock serves scripted server-sent event replies in order and records the
//! request bodies, so each test can check what the provider sent.
use axum::{
    Json, Router,
    extract::State,
    http::header,
    response::{IntoResponse, Response},
    routing::post,
};
use bubblepaw_core::{
    CancellationToken, Message, ModelRequest, Provider, ProviderError, ProviderErrorKind, Role,
    TextCallback, ToolDefinition, Usage,
};
use bubblepaw_runner::provider::{ChatCompletionsProvider, MessagesProvider, ResponsesProvider};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

const PROMPT: &str = "Continue exactly where you stopped.";

#[derive(Clone, Default)]
struct Mock {
    replies: Arc<Mutex<VecDeque<String>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn handle(State(mock): State<Mock>, Json(body): Json<Value>) -> Response {
    mock.requests.lock().unwrap().push(body);
    let reply = mock.replies.lock().unwrap().pop_front().unwrap_or_default();
    ([(header::CONTENT_TYPE, "text/event-stream")], reply).into_response()
}

/// Starts the mock and returns its root URL and a handle to its recorded requests.
async fn start(replies: Vec<String>) -> (String, Mock) {
    let mock = Mock {
        replies: Arc::new(Mutex::new(replies.into())),
        requests: Arc::default(),
    };
    let app = Router::new()
        .route("/messages", post(handle))
        .route("/chat/completions", post(handle))
        .route("/responses", post(handle))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, mock)
}

/// Server-sent event data lines, one per value.
fn sse(values: &[Value]) -> String {
    values
        .iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect()
}

/// Chat completion events, ended by the `[DONE]` sentinel.
fn chat_sse(values: &[Value]) -> String {
    format!("{}data: [DONE]\n\n", sse(values))
}

fn user(text: &str) -> Message {
    Message {
        id: "fixture-message".into(),
        session_id: "fixture-session".into(),
        role: Role::User,
        origin: None,
        content: text.into(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: String::new(),
    }
}

fn request(model: &str, max_output_tokens: Option<u32>) -> ModelRequest {
    ModelRequest {
        model: model.into(),
        system_prompt: "test".into(),
        messages: vec![user("Write the answer.")],
        max_output_tokens,
        ..Default::default()
    }
}

fn workspace_read_tool() -> ToolDefinition {
    ToolDefinition {
        approval: Default::default(),
        name: "workspace_read".into(),
        description: "read".into(),
        read_only: true,
        input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
    }
}

fn noop() -> TextCallback {
    Arc::new(|_: &str| {})
}

// Messages replies.

fn messages_reply(text: &str, stop: &str, output: u64) -> String {
    sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_sequence":null},"usage":{"output_tokens":output}}),
        json!({"type":"message_stop"}),
    ])
}

/// Text, then a tool call whose input is cut off by the length stop.
fn messages_cut_in_tool(text: &str) -> String {
    sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_cut","name":"workspace_read","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"a"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":60}}),
        json!({"type":"message_stop"}),
    ])
}

#[tokio::test]
async fn messages_length_stop_is_continued_and_joined() {
    let (base, mock) = start(vec![
        messages_reply("Part one ", "max_tokens", 50),
        messages_reply("part two.", "end_turn", 7),
    ])
    .await;
    let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
    let response = provider
        .generate(
            request("claude-haiku-5-5", None),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.text, "Part one part two.");
    assert!(response.tool_calls.is_empty());
    let usage = response.usage.unwrap();
    assert_eq!(usage.input_tokens, 20, "input usage sums across requests");
    let last = response.last_request_usage.unwrap();
    assert_eq!(last.input_tokens, Some(10));
    assert_eq!(last.output_tokens, Some(7));
    assert_eq!(usage.output_tokens, 57, "output usage sums across requests");
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["max_tokens"], 128_000);
    let messages = requests[1]["messages"].as_array().unwrap();
    let last = messages.len() - 1;
    assert_eq!(messages[last]["role"], "user");
    assert_eq!(messages[last]["content"][0]["text"], PROMPT);
    assert_eq!(messages[last - 1]["role"], "assistant");
    assert_eq!(messages[last - 1]["content"][0]["text"], "Part one ");
    assert_eq!(messages[last - 1]["content"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn messages_cut_inside_a_tool_call_drops_it_and_continues() {
    let (base, mock) = start(vec![
        messages_cut_in_tool("Let me read. "),
        messages_reply("Done.", "end_turn", 5),
    ])
    .await;
    let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
    let mut request = request("claude-haiku-5-5", None);
    request.tools = vec![workspace_read_tool()];
    let response = provider
        .generate(request, noop(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.text, "Let me read. Done.");
    assert!(
        response.tool_calls.is_empty(),
        "the cut tool call is dropped"
    );
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let text = requests[1].to_string();
    assert!(
        !text.contains("tool_use"),
        "no partial tool call is sent back"
    );
    assert!(!text.contains("toolu_cut"));
}

#[tokio::test]
async fn messages_continuation_limit_returns_incomplete_not_an_error() {
    let (base, mock) = start(
        (0..5)
            .map(|_| messages_reply("x", "max_tokens", 9))
            .collect(),
    )
    .await;
    let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
    let response = provider
        .generate(
            request("claude-haiku-5-5", None),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.text, "xxxxx");
    assert_eq!(
        mock.requests.lock().unwrap().len(),
        5,
        "one request plus four continuations"
    );
}

#[tokio::test]
async fn messages_refusal_stays_an_error() {
    let (base, _mock) = start(vec![messages_reply("No.", "refusal", 3)]).await;
    let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
    let error = provider
        .generate(
            request("claude-haiku-5-5", None),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("refused"), "{error}");
}

#[tokio::test]
async fn messages_output_limit_comes_from_the_table_or_the_role() {
    let (base, mock) = start(vec![
        messages_reply("a", "end_turn", 1),
        messages_reply("b", "end_turn", 1),
        messages_reply("c", "end_turn", 1),
    ])
    .await;
    let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
    provider
        .generate(
            request("claude-haiku-5-5", None),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    provider
        .generate(
            request("some-other-model", None),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    provider
        .generate(
            request("claude-haiku-5-5", Some(1000)),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests[0]["max_tokens"], 128_000);
    assert_eq!(requests[1]["max_tokens"], 64_000);
    assert_eq!(requests[2]["max_tokens"], 1000);
}

// Responses replies.

/// Text, then a stop on the output limit. `output` lists the items the cut left.
fn responses_cut(text: &str, output: Value) -> String {
    sse(&[
        json!({"type":"response.output_text.delta","delta":text}),
        json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":output,"usage":{"input_tokens":5,"output_tokens":30}}}),
    ])
}

fn responses_done(text: &str) -> String {
    sse(&[
        json!({"type":"response.output_text.delta","delta":text}),
        json!({"type":"response.completed","response":{"status":"completed","output":[],"usage":{"input_tokens":6,"output_tokens":4}}}),
    ])
}

#[tokio::test]
async fn responses_length_stop_is_continued_and_joined() {
    let (base, mock) = start(vec![
        responses_cut("Part one ", json!([])),
        responses_done("part two."),
    ])
    .await;
    let provider = ResponsesProvider::api_key("synthetic-key", base).unwrap();
    let response = provider
        .generate(
            request("gpt-6-luna", None),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.text, "Part one part two.");
    let usage = response.usage.unwrap();
    assert_eq!(usage.input_tokens, 11);
    let last = response.last_request_usage.unwrap();
    assert_eq!(last.input_tokens, Some(6));
    assert_eq!(last.output_tokens, Some(4));
    assert_eq!(usage.output_tokens, 34);
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for body in requests.iter() {
        assert!(
            body.get("max_output_tokens").is_none(),
            "no output limit by default"
        );
    }
    let input = requests[1]["input"].as_array().unwrap();
    let last = input.len() - 1;
    assert_eq!(input[last]["content"][0]["text"], PROMPT);
    assert_eq!(input[last - 1]["role"], "assistant");
    assert_eq!(input[last - 1]["content"][0]["text"], "Part one ");
}

#[tokio::test]
async fn responses_cut_inside_a_function_call_drops_it_and_continues() {
    let partial_call = json!([{"type":"function_call","call_id":"call_cut","name":"workspace_read","arguments":"{\"path\": \"a"}]);
    let (base, mock) = start(vec![
        responses_cut("Working ", partial_call),
        responses_done("done."),
    ])
    .await;
    let provider = ResponsesProvider::api_key("synthetic-key", base).unwrap();
    let response = provider
        .generate(
            request("gpt-6-luna", None),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.text, "Working done.");
    assert!(response.tool_calls.is_empty(), "the cut call is dropped");
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(!requests[1].to_string().contains("call_cut"));
}

#[tokio::test]
async fn responses_continuation_limit_returns_incomplete_not_an_error() {
    let (base, mock) = start((0..5).map(|_| responses_cut("x", json!([]))).collect()).await;
    let provider = ResponsesProvider::api_key("synthetic-key", base).unwrap();
    let response = provider
        .generate(
            request("gpt-6-luna", None),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.text, "xxxxx");
    assert_eq!(mock.requests.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn responses_sends_a_limit_only_when_the_role_sets_one() {
    let (base, mock) = start(vec![responses_done("a"), responses_done("b")]).await;
    let provider = ResponsesProvider::api_key("synthetic-key", base).unwrap();
    provider
        .generate(
            request("gpt-6-luna", None),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    provider
        .generate(
            request("gpt-6-luna", Some(500)),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let requests = mock.requests.lock().unwrap();
    assert!(requests[0].get("max_output_tokens").is_none());
    assert_eq!(requests[1]["max_output_tokens"], 500);
}

// Chat completions replies.

fn chat_cut(text: &str) -> String {
    chat_sse(&[
        json!({"choices":[{"index":0,"delta":{"content":text}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":40,"completion_tokens_details":{"reasoning_tokens":0}}}),
    ])
}

fn chat_done(text: &str) -> String {
    chat_sse(&[
        json!({"choices":[{"index":0,"delta":{"content":text}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":5,"completion_tokens_details":{"reasoning_tokens":2}}}),
    ])
}

#[tokio::test]
async fn malformed_continuation_retains_every_reported_physical_response() {
    let (messages_base, messages_mock) = start(vec![
        messages_reply("partial", "max_tokens", 50),
        sse(&[
            json!({"type":"message_start","message":{"usage":{"input_tokens":20,"output_tokens":1,"cache_read_input_tokens":3,"cache_creation_input_tokens":2}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"bad","name":"unavailable","input":{}}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5,"reasoning_tokens":1}}),
            json!({"type":"message_stop"}),
        ]),
    ]).await;
    let (responses_base, responses_mock) = start(vec![
        responses_cut("partial", json!([])),
        sse(&[json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call","call_id":"bad","name":"unavailable","arguments":"{}"}],"usage":{"input_tokens":6,"output_tokens":4,"input_tokens_details":{"cached_tokens":3,"cache_creation_tokens":2},"output_tokens_details":{"reasoning_tokens":1}}}})]),
    ]).await;
    let (chat_base, chat_mock) = start(vec![
        chat_cut("partial"),
        chat_sse(&[
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"bad","type":"function","function":{"name":"unavailable","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":4,"cache_creation_tokens":2},"completion_tokens_details":{"reasoning_tokens":1}}}),
        ]),
    ]).await;
    let cases: Vec<(Arc<dyn Provider>, Mock, Usage)> = vec![
        (
            Arc::new(MessagesProvider::api_key("synthetic-key", messages_base).unwrap()),
            messages_mock,
            Usage {
                input_tokens: 35,
                output_tokens: 55,
                cache_read_tokens: 3,
                cache_creation_tokens: 2,
                reasoning_tokens: Some(1),
            },
        ),
        (
            Arc::new(ResponsesProvider::api_key("synthetic-key", responses_base).unwrap()),
            responses_mock,
            Usage {
                input_tokens: 11,
                output_tokens: 34,
                cache_read_tokens: 3,
                cache_creation_tokens: 2,
                reasoning_tokens: Some(1),
            },
        ),
        (
            Arc::new(ChatCompletionsProvider::api_key("synthetic-key", chat_base).unwrap()),
            chat_mock,
            Usage {
                input_tokens: 19,
                output_tokens: 43,
                cache_read_tokens: 4,
                cache_creation_tokens: 2,
                reasoning_tokens: Some(1),
            },
        ),
    ];
    for (provider, mock, expected) in cases {
        let mut request = request("fixture-model", None);
        request.tools = vec![workspace_read_tool()];
        let (result, records) = bubblepaw_core::capture_request_usage(
            "fixture-session",
            Some("fixture-run"),
            bubblepaw_core::RequestKind::Main,
            provider.generate(request, noop(), CancellationToken::new()),
        )
        .await;
        let error = result.unwrap_err();
        assert_eq!(records.len(), 2, "one row per physical continuation");
        assert!(!records[0].failed);
        assert!(records[1].failed);
        let mut recorded = Usage::default();
        for record in &records {
            recorded.add(&record.usage);
            assert_eq!(record.session_id, "fixture-session");
            assert_eq!(record.run_id.as_deref(), Some("fixture-run"));
        }
        assert_eq!(
            recorded, expected,
            "failed row retains only its own billable usage"
        );
        assert_ne!(records[0].request_id, records[1].request_id);
        let classified = ProviderError::of(&error).unwrap();
        assert_eq!(classified.kind, ProviderErrorKind::Fatal);
        assert!(classified.published);
        assert_eq!(classified.usage.as_ref(), Some(&expected));
        assert_eq!(mock.requests.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn chat_length_stop_is_continued_and_joined() {
    let (base, mock) = start(vec![chat_cut("Part one "), chat_done("part two.")]).await;
    let provider = ChatCompletionsProvider::api_key("synthetic-key", base).unwrap();
    let response = provider
        .generate(
            request("episode-model", None),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.text, "Part one part two.");
    let usage = response.usage.unwrap();
    assert_eq!(usage.input_tokens, 16);
    let last = response.last_request_usage.unwrap();
    assert_eq!(last.input_tokens, Some(8));
    assert_eq!(last.output_tokens, Some(5));
    assert_eq!(usage.output_tokens, 45);
    assert_eq!(
        usage.reasoning_tokens,
        Some(2),
        "known zero plus two remains known and additive"
    );
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0].get("max_tokens").is_none(),
        "no output limit by default"
    );
    assert!(requests[1].get("max_tokens").is_none());
    let messages = requests[1]["messages"].as_array().unwrap();
    let last = messages.len() - 1;
    assert_eq!(messages[last]["role"], "user");
    assert_eq!(messages[last]["content"], PROMPT);
    assert_eq!(messages[last - 1]["role"], "assistant");
    assert_eq!(messages[last - 1]["content"], "Part one ");
}

#[tokio::test]
async fn final_continuation_without_input_usage_stays_unknown_for_every_http_adapter() {
    fn without_usage(reply: String) -> String {
        reply
            .lines()
            .map(|line| {
                if let Some(payload) = line.strip_prefix("data: ")
                    && let Ok(mut value) = serde_json::from_str::<Value>(payload)
                {
                    value.as_object_mut().unwrap().remove("usage");
                    if let Some(response) = value.get_mut("response").and_then(Value::as_object_mut)
                    {
                        response.remove("usage");
                    }
                    if let Some(message) = value.get_mut("message").and_then(Value::as_object_mut) {
                        message.remove("usage");
                    }
                    return format!("data: {value}");
                }
                line.into()
            })
            .collect::<Vec<String>>()
            .join("\n")
            + "\n"
    }
    let (base, _) = start(vec![
        messages_reply("one", "max_tokens", 30),
        without_usage(messages_reply("two", "end_turn", 4)),
    ])
    .await;
    let messages: Arc<dyn Provider> =
        Arc::new(MessagesProvider::api_key("synthetic-key", base).unwrap());
    let (base, _) = start(vec![
        responses_cut("one", json!([])),
        without_usage(responses_done("two")),
    ])
    .await;
    let responses: Arc<dyn Provider> =
        Arc::new(ResponsesProvider::api_key("synthetic-key", base).unwrap());
    let (base, _) = start(vec![chat_cut("one"), without_usage(chat_done("two"))]).await;
    let chat: Arc<dyn Provider> =
        Arc::new(ChatCompletionsProvider::api_key("synthetic-key", base).unwrap());
    for (provider, expected_input) in [(messages, 10), (responses, 5), (chat, 8)] {
        let response = provider
            .generate(
                request("fixture-model", None),
                noop(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let last = response.last_request_usage.unwrap();
        assert_eq!(last.input_tokens, None);
        assert_eq!(last.output_tokens, None);
        assert_eq!(
            response.usage.unwrap().input_tokens,
            expected_input,
            "earlier known telemetry remains cumulative"
        );
    }
}

#[tokio::test]
async fn chat_cut_inside_a_function_call_drops_it_and_continues() {
    let cut = chat_sse(&[
        json!({"choices":[{"index":0,"delta":{"content":"Working "}}]}),
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_cut","type":"function","function":{"name":"workspace_read","arguments":"{\"path\":\"a"}}]}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":40}}),
    ]);
    let (base, mock) = start(vec![cut, chat_done("done.")]).await;
    let provider = ChatCompletionsProvider::api_key("synthetic-key", base).unwrap();
    let mut input = request("synthetic-tools", None);
    input.tools = vec![workspace_read_tool()];
    let response = provider
        .generate(input, noop(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.text, "Working done.");
    assert!(
        response.tool_calls.is_empty(),
        "the cut call is never executable"
    );
    assert_eq!(response.usage.unwrap().output_tokens, 45);
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(!requests[1].to_string().contains("call_cut"));
    let messages = requests[1]["messages"].as_array().unwrap();
    assert_eq!(
        messages[messages.len() - 2],
        json!({"role":"assistant","content":"Working "})
    );
    assert_eq!(messages.last().unwrap()["content"], PROMPT);
}

#[tokio::test]
async fn chat_continuation_limit_returns_incomplete_not_an_error() {
    let (base, mock) = start((0..5).map(|_| chat_cut("x")).collect()).await;
    let provider = ChatCompletionsProvider::api_key("synthetic-key", base).unwrap();
    let response = provider
        .generate(
            request("episode-model", None),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.text, "xxxxx");
    assert_eq!(mock.requests.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn chat_sends_a_limit_only_when_the_role_sets_one() {
    let (base, mock) = start(vec![chat_done("a")]).await;
    let provider = ChatCompletionsProvider::api_key("synthetic-key", base).unwrap();
    provider
        .generate(
            request("episode-model", Some(77)),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests[0]["max_tokens"], 77);
}

#[tokio::test]
async fn stream_errors_are_not_continued_and_keep_their_kind() {
    let (base, _mock) = start(vec![sse(&[
        json!({"type":"error","error":{"type":"invalid_request_error"}}),
    ])])
    .await;
    let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
    let error = provider
        .generate(
            request("claude-haiku-5-5", None),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        ProviderError::of(&error).map(|error| error.kind.clone()),
        Some(ProviderErrorKind::Fatal)
    );
}

// Round-one reasoning travels with the continuation and is stored once.

/// Exact count of `needle` in the serialized value.
fn occurrences(value: &Value, needle: &str) -> usize {
    value.to_string().matches(needle).count()
}

/// Messages stream events for arbitrary native blocks, in order.
fn messages_blocks(blocks: &[Value], stop: &str) -> String {
    let mut events = vec![
        json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1}}}),
    ];
    for (index, block) in blocks.iter().enumerate() {
        match block["type"].as_str().unwrap() {
            "thinking" => {
                let mut start = block.clone();
                start["thinking"] = json!("");
                start["signature"] = json!("");
                events.push(
                    json!({"type":"content_block_start","index":index,"content_block":start}),
                );
                events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"thinking_delta","thinking":block["thinking"]}}));
                events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"signature_delta","signature":block["signature"]}}));
            }
            "text" => {
                events.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}));
                events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":block["text"]}}));
            }
            _ => events
                .push(json!({"type":"content_block_start","index":index,"content_block":block})),
        }
        events.push(json!({"type":"content_block_stop","index":index}));
    }
    events.push(json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_sequence":null},"usage":{"output_tokens":5}}));
    events.push(json!({"type":"message_stop"}));
    sse(&events)
}

#[tokio::test]
async fn messages_continuation_carries_round_one_reasoning_and_stores_all_rounds_once() {
    let thinking_one = json!({"type":"thinking","thinking":"Plan π","signature":"sig-one+/==猫"});
    let redacted_one = json!({"type":"redacted_thinking","data":"cipher-one+/=="});
    let text_one = json!({"type":"text","text":"Part one "});
    let thinking_two = json!({"type":"thinking","thinking":"Resume","signature":"sig-two+/=="});
    let (base, mock) = start(vec![
        messages_blocks(
            &[thinking_one.clone(), redacted_one.clone(), text_one.clone()],
            "max_tokens",
        ),
        messages_blocks(
            &[
                thinking_two.clone(),
                json!({"type":"text","text":"part two."}),
            ],
            "end_turn",
        ),
    ])
    .await;
    let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
    let response = provider
        .generate(
            request("claude-haiku-5-5", None),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.text, "Part one part two.");
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let messages = requests[1]["messages"].as_array().unwrap();
    let partial = &messages[messages.len() - 2];
    assert_eq!(partial["role"], "assistant");
    assert_eq!(
        partial["content"],
        json!([thinking_one, redacted_one, text_one]),
        "round-one reasoning keeps its original blocks and positions"
    );
    assert_eq!(messages[messages.len() - 1]["content"][0]["text"], PROMPT);
    let state = response.provider_state.unwrap();
    for needle in ["sig-one+/==猫", "cipher-one+/==", "sig-two+/=="] {
        assert_eq!(
            occurrences(&state, needle),
            1,
            "{needle} stored exactly once"
        );
    }
    let kinds: Vec<_> = state["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| block["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["thinking", "redacted_thinking", "text", "thinking", "text"]
    );
}

#[tokio::test]
async fn responses_continuation_carries_round_one_reasoning_and_stores_all_rounds_once() {
    let reasoning_one = json!({"type":"reasoning","id":"rs_one","encrypted_content":"enc-one+/==猫","summary":[{"type":"summary_text","text":"plan"}]});
    let message_one = json!({"type":"message","id":"msg_one","role":"assistant","status":"incomplete","content":[{"type":"output_text","text":"Part one ","annotations":[]}]});
    let reasoning_two =
        json!({"type":"reasoning","id":"rs_two","encrypted_content":"enc-two+/==","summary":[]});
    let message_two = json!({"type":"message","id":"msg_two","role":"assistant","status":"completed","content":[{"type":"output_text","text":"part two.","annotations":[]}]});
    let (base, mock) = start(vec![
        responses_cut(
            "Part one ",
            json!([reasoning_one.clone(), message_one.clone()]),
        ),
        sse(&[
            json!({"type":"response.output_text.delta","delta":"part two."}),
            json!({"type":"response.completed","response":{"status":"completed","output":[reasoning_two.clone(), message_two.clone()],"usage":{"input_tokens":6,"output_tokens":4}}}),
        ]),
    ])
    .await;
    let provider = ResponsesProvider::api_key("synthetic-key", base).unwrap();
    let response = provider
        .generate(
            request("gpt-6-luna", None),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.text, "Part one part two.");
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let input = requests[1]["input"].as_array().unwrap();
    let last = input.len() - 1;
    assert_eq!(input[last]["content"][0]["text"], PROMPT);
    assert_eq!(
        input[last - 2..last],
        [reasoning_one, message_one],
        "round-one reasoning precedes its partial message, unchanged"
    );
    let state = response.provider_state.unwrap();
    for needle in ["enc-one+/==猫", "enc-two+/==", "rs_one", "rs_two"] {
        assert_eq!(
            occurrences(&state, needle),
            1,
            "{needle} stored exactly once"
        );
    }
    let kinds: Vec<_> = state["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["reasoning", "message", "reasoning", "message"]);
}

/// Chat events with reasoning, cut or finished.
fn chat_reasoning(reasoning: &str, text: &str, finish: &str) -> String {
    chat_sse(&[
        json!({"choices":[{"index":0,"delta":{"reasoning_content":reasoning}}]}),
        json!({"choices":[{"index":0,"delta":{"content":text}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]}),
        json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":5}}),
    ])
}

#[tokio::test]
async fn chat_continuation_stores_reasoning_from_every_round() {
    let (base, mock) = start(vec![
        chat_reasoning("think one", "Part one ", "length"),
        chat_reasoning("think two", "part two.", "stop"),
    ])
    .await;
    let provider = ChatCompletionsProvider::api_key("synthetic-key", base).unwrap();
    let response = provider
        .generate(
            request("episode-model", None),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.text, "Part one part two.");
    let state = response.provider_state.unwrap();
    for needle in ["think one", "think two"] {
        assert_eq!(
            occurrences(&state, needle),
            1,
            "{needle} stored exactly once"
        );
    }
    // Default replay is none: the continuation keeps the historical text-only shape.
    let requests = mock.requests.lock().unwrap();
    assert!(!requests[1].to_string().contains("think one"));
    let messages = requests[1]["messages"].as_array().unwrap();
    assert_eq!(
        messages[messages.len() - 2],
        json!({"role":"assistant","content":"Part one "})
    );
}

#[tokio::test]
async fn messages_continuation_omits_a_thinking_block_cut_before_its_signature() {
    let signed = json!({"type":"thinking","thinking":"Plan","signature":"sig-kept+/=="});
    let text_one = json!({"type":"text","text":"Part one "});
    let unsigned = json!({"type":"thinking","thinking":"Half thought","signature":""});
    let (base, mock) = start(vec![
        messages_blocks(&[signed.clone(), text_one.clone(), unsigned], "max_tokens"),
        messages_blocks(&[json!({"type":"text","text":"part two."})], "end_turn"),
    ])
    .await;
    let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
    let response = provider
        .generate(
            request("claude-haiku-5-5", None),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.text, "Part one part two.");
    let requests = mock.requests.lock().unwrap();
    let messages = requests[1]["messages"].as_array().unwrap();
    // The signed block still goes back; the unsigned partial one does not.
    assert_eq!(
        messages[messages.len() - 2]["content"],
        json!([signed, text_one])
    );
    assert!(!requests[1].to_string().contains("Half thought"));
    // The unsigned block is kept with the reply, byte-exact and marked as withheld.
    let state = response.provider_state.unwrap();
    assert_eq!(occurrences(&state, "sig-kept+/=="), 1);
    assert_eq!(occurrences(&state, "Half thought"), 1);
    assert_eq!(
        state["content"][2],
        json!({"type":"thinking","thinking":"Half thought","signature":"","unsigned_cut":true})
    );
}

#[tokio::test]
async fn messages_withheld_thinking_stays_out_of_tool_rounds_and_next_turns() {
    let signed = json!({"type":"thinking","thinking":"Plan","signature":"sig-kept+/=="});
    let text_one = json!({"type":"text","text":"Part one "});
    let unsigned = json!({"type":"thinking","thinking":"Half thought","signature":""});
    let call = json!({"type":"tool_use","id":"toolu_read","name":"workspace_read","input":{"path":"a.txt"}});
    let (base, mock) = start(vec![
        messages_blocks(&[signed, text_one.clone(), unsigned], "max_tokens"),
        sse(&[
            json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_read","name":"workspace_read","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"a.txt\"}"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":5}}),
            json!({"type":"message_stop"}),
        ]),
        messages_blocks(&[json!({"type":"text","text":"Read it."})], "end_turn"),
        messages_blocks(&[json!({"type":"text","text":"Next answer."})], "end_turn"),
    ])
    .await;
    let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
    let mut first = request("claude-haiku-5-5", None);
    first.tools = vec![workspace_read_tool()];
    let round = provider
        .generate(first.clone(), noop(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(round.tool_calls.len(), 1);
    let stored = round.provider_state.clone().unwrap();
    assert_eq!(
        stored["content"][2],
        json!({"type":"thinking","thinking":"Half thought","signature":"","unsigned_cut":true})
    );
    // The next tool round replays the stored reply without the withheld block.
    let mut tool_round = first;
    tool_round.messages.push(Message {
        role: Role::Assistant,
        content: round.text.clone(),
        tool_calls: round.tool_calls.clone(),
        provider_state: round.provider_state.clone(),
        ..user("")
    });
    tool_round.messages.push(Message {
        role: Role::Tool,
        content: "alpha".into(),
        tool_call_id: Some("toolu_read".into()),
        ..user("")
    });
    let tool_done = provider
        .generate(tool_round.clone(), noop(), CancellationToken::new())
        .await
        .unwrap();
    // A later user turn replays the same stored reply, still without it.
    let mut next_turn = tool_round;
    next_turn.messages.push(Message {
        role: Role::Assistant,
        content: tool_done.text.clone(),
        provider_state: tool_done.provider_state.clone(),
        ..user("")
    });
    next_turn.messages.push(user("Next question."));
    provider
        .generate(next_turn, noop(), CancellationToken::new())
        .await
        .unwrap();
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for later in [&requests[2], &requests[3]] {
        assert!(!later.to_string().contains("Half thought"));
        assert_eq!(
            later["messages"][1]["content"],
            json!([
                {"type":"thinking","thinking":"Plan","signature":"sig-kept+/=="},
                text_one,
                call
            ])
        );
    }
}

#[tokio::test]
async fn physical_continuation_rows_commit_with_the_assistant() -> anyhow::Result<()> {
    use bubblepaw_core::{Harness, HarnessConfig, RunOptions, Store, ToolRegistry};
    let (chat_base, _) = start(vec![chat_cut("Part one "), chat_done("part two.")]).await;
    let (messages_base, _) = start(vec![
        messages_reply("Part one ", "max_tokens", 50),
        messages_reply("part two.", "end_turn", 7),
    ])
    .await;
    let (responses_base, _) = start(vec![
        responses_cut("Part one ", json!([])),
        responses_done("part two."),
    ])
    .await;
    let cases: Vec<(&str, Arc<dyn Provider>)> = vec![
        (
            "chat-completions",
            Arc::new(ChatCompletionsProvider::api_key("fixture", chat_base)?),
        ),
        (
            "anthropic-messages",
            Arc::new(MessagesProvider::api_key("fixture", messages_base)?),
        ),
        (
            "responses",
            Arc::new(ResponsesProvider::api_key("fixture", responses_base)?),
        ),
    ];
    for (provider_name, provider) in cases {
        let dir = tempfile::tempdir()?;
        let store = Store::open_data_dir(dir.path())?;
        let session = store.create_session("physical usage")?.id;
        let harness = Harness::new(
            store.clone(),
            provider,
            ToolRegistry::new(),
            HarnessConfig {
                model: "fixture-model".into(),
                ..Default::default()
            },
        );
        let before = store.storage_stats()?;
        let run = harness
            .prompt(&session, "Continue", RunOptions::default())
            .await?;
        assert_eq!(run.status, "completed", "{provider_name}: {:?}", run.error);
        let rows = store.request_usage()?;
        assert_eq!(rows.len(), 2, "{provider_name}");
        let mut total = Usage::default();
        for row in &rows {
            assert_eq!(row.provider, provider_name);
            assert_eq!(row.model, "fixture-model");
            assert_eq!(row.session_id, session);
            assert_eq!(row.run_id.as_deref(), Some(run.id.as_str()));
            assert_eq!(row.kind, bubblepaw_core::RequestKind::Main);
            assert!(!row.failed);
            total.add(&row.usage);
        }
        assert_eq!(run.usage.as_ref(), Some(&total));
        let after = store.storage_stats()?;
        assert_eq!(
            after.chat_transaction_count - before.chat_transaction_count,
            2,
            "continuations add no usage-only transactions"
        );
        drop(harness);
        drop(store);
        assert_eq!(Store::read_request_usage_data_dir(dir.path())?.len(), 2);
    }
    Ok(())
}
