//! Replay policy for stored reasoning: chat's per-model `reasoning_replay`
//! setting, and foreign reasoning returning to its origin provider. Loopback
//! HTTP with synthetic data only.
use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::State,
    http::header,
    response::{IntoResponse, Response},
    routing::post,
};
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, Message, ModelRequest, Provider, ReasoningReplay,
    Role, RunOptions, Store, TextCallback, Tool, ToolContext, ToolDefinition, ToolRegistry,
    ToolResult,
};
use bubblepaw_runner::provider::{ChatCompletionsProvider, MessagesProvider, ResponsesProvider};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

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

fn sse(values: &[Value]) -> String {
    values
        .iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect()
}

fn noop() -> TextCallback {
    Arc::new(|_: &str| {})
}

fn message(role: Role, content: &str) -> Message {
    Message {
        id: format!("m-{content}"),
        session_id: "synthetic-session".into(),
        role,
        origin: None,
        content: content.into(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: String::new(),
    }
}

fn assistant(content: &str, state: Option<Value>) -> Message {
    let mut message = message(Role::Assistant, content);
    message.provider_state = state;
    message
}

fn request(model: &str, messages: Vec<Message>) -> ModelRequest {
    ModelRequest {
        model: model.into(),
        system_prompt: "test".into(),
        messages,
        ..Default::default()
    }
}

// Chat completions.

fn chat_reply(reasoning: &str, text: &str) -> String {
    format!(
        "{}data: [DONE]\n\n",
        sse(&[
            json!({"choices":[{"index":0,"delta":{"reasoning_content":reasoning}}]}),
            json!({"choices":[{"index":0,"delta":{"content":text}}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":5}}),
        ])
    )
}

const CHAT_MODEL: &str = "synthetic-reasoner";

fn chat_tool() -> ToolDefinition {
    ToolDefinition {
        approval: Default::default(),
        name: "read".into(),
        description: "read".into(),
        read_only: true,
        input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
    }
}

fn chat_tool_reply() -> String {
    format!(
        "{}data: [DONE]\n\n",
        sse(&[
            json!({"choices":[{"index":0,"delta":{"reasoning_content":"R-loop","content":"Calling"}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}}]}}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":5}}),
        ])
    )
}

/// One closed turn and one unfinished tool loop, both with stored reasoning.
async fn chat_history(with_tools: bool) -> Vec<Message> {
    let (base, _mock) = start(vec![
        chat_reply("R-closed", "Answer one"),
        if with_tools {
            chat_tool_reply()
        } else {
            chat_reply("R-loop", "Calling")
        },
    ])
    .await;
    let provider = ChatCompletionsProvider::api_key("synthetic-key", base).unwrap();
    let mut responses = Vec::new();
    for index in 0..2 {
        let mut input = request(CHAT_MODEL, vec![message(Role::User, "q")]);
        if with_tools && index == 1 {
            input.tools = vec![chat_tool()];
        }
        let response = provider
            .generate(input, noop(), CancellationToken::new())
            .await
            .unwrap();
        responses.push(response);
    }
    let response = responses.pop().unwrap();
    let mut call = assistant(&response.text, response.provider_state);
    call.tool_calls = response.tool_calls;
    let mut result = message(Role::Tool, "file a");
    result.tool_call_id = Some("call-1".into());
    let first = responses.pop().unwrap();
    let mut history = vec![
        message(Role::User, "first"),
        assistant(&first.text, first.provider_state),
        message(Role::User, "second"),
        call,
    ];
    if with_tools {
        history.push(result);
    }
    history
}

async fn chat_body(policy: Option<ReasoningReplay>, model: &str, with_tools: bool) -> Value {
    let history = chat_history(with_tools).await;
    let (base, mock) = start(vec![chat_reply("R-next", "done")]).await;
    let mut provider = ChatCompletionsProvider::api_key("synthetic-key", base).unwrap();
    if let Some(policy) = policy {
        provider = provider
            .with_reasoning_replay(BTreeMap::from([(CHAT_MODEL.to_owned(), policy)]))
            .unwrap();
    }
    provider
        .generate(request(model, history), noop(), CancellationToken::new())
        .await
        .unwrap();
    mock.requests.lock().unwrap().remove(0)
}

#[tokio::test]
async fn chat_stores_reasoning_in_every_reply() {
    let history = chat_history(true).await;
    assert_eq!(
        history[1].provider_state.as_ref().unwrap()["message_fields"]["reasoning_content"],
        "R-closed"
    );
    assert_eq!(
        history[3].provider_state.as_ref().unwrap()["message_fields"]["reasoning_content"],
        "R-loop"
    );
}

#[tokio::test]
async fn chat_replay_none_keeps_the_historical_wire_messages() {
    let legacy = json!([
        {"role":"system","content":"test"},
        {"role":"user","content":"first"},
        {"role":"assistant","content":"Answer one"},
        {"role":"user","content":"second"},
        {"role":"assistant","content":"Calling"},
    ]);
    for policy in [None, Some(ReasoningReplay::None)] {
        assert_eq!(
            chat_body(policy, CHAT_MODEL, false).await["messages"],
            legacy
        );
    }
    // A setting for another model leaves a completed turn without replay.
    assert_eq!(
        chat_body(Some(ReasoningReplay::All), "other-model", false).await["messages"],
        legacy
    );
}

#[tokio::test]
async fn chat_replay_tool_loop_sends_only_the_open_loop() {
    for policy in [None, Some(ReasoningReplay::ToolLoop)] {
        let body = chat_body(policy, CHAT_MODEL, true).await;
        let messages = &body["messages"];
        assert_eq!(
            messages[2],
            json!({"role":"assistant","content":"Answer one"})
        );
        assert_eq!(
            messages[4],
            json!({"role":"assistant","content":"Calling","reasoning_content":"R-loop","tool_calls":[{"id":"call-1","type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}}]})
        );
    }
}

#[tokio::test]
async fn generated_host_notice_keeps_current_chat_tool_loop_reasoning() {
    let mut history = chat_history(true).await;
    let mut notice = message(Role::User, "[host notice]\nSynthetic job completed.");
    notice.origin = Some(bubblepaw_core::MessageOrigin::Generated);
    history.push(notice);
    let (base, mock) = start(vec![chat_reply("R-next", "done")]).await;
    ChatCompletionsProvider::api_key("synthetic-key", base)
        .unwrap()
        .generate(
            request(CHAT_MODEL, history),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let body = mock.requests.lock().unwrap().remove(0);
    assert_eq!(body["messages"][4]["reasoning_content"], "R-loop");
    assert!(body["messages"][2].get("reasoning_content").is_none());
}

#[tokio::test]
async fn chat_replay_all_sends_every_stored_reasoning() {
    let body = chat_body(Some(ReasoningReplay::All), CHAT_MODEL, true).await;
    let messages = &body["messages"];
    assert_eq!(
        messages[2],
        json!({"role":"assistant","content":"Answer one","reasoning_content":"R-closed"})
    );
    assert_eq!(
        messages[4],
        json!({"role":"assistant","content":"Calling","reasoning_content":"R-loop","tool_calls":[{"id":"call-1","type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}}]})
    );
}

#[derive(Default)]
struct ReadTool(AtomicUsize);
#[async_trait]
impl Tool for ReadTool {
    fn definition(&self) -> ToolDefinition {
        chat_tool()
    }
    async fn invoke(&self, _: ToolContext, input: Value) -> anyhow::Result<ToolResult> {
        assert_eq!(input, json!({"path":"a"}));
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text("file a"))
    }
}

#[tokio::test]
async fn chat_tool_loop_replays_reasoning_through_an_executed_tool_and_tools_free_final_round() {
    let (base, mock) = start(vec![
        chat_reply("R-closed", "Answer one"),
        chat_tool_reply(),
        chat_reply("R-final", "done"),
    ])
    .await;
    let provider = ChatCompletionsProvider::api_key("synthetic-key", base)
        .unwrap()
        .with_reasoning_replay(BTreeMap::from([(
            CHAT_MODEL.into(),
            ReasoningReplay::ToolLoop,
        )]))
        .unwrap();
    let tools = Arc::new(ReadTool::default());
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path().join("archive.sqlite")).unwrap();
    let session = store.create_session("reasoning").unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(tools.clone()).unwrap();
    let harness = Harness::new(
        store.clone(),
        Arc::new(provider),
        registry,
        HarnessConfig {
            system_prompt: "test".into(),
            ..Default::default()
        },
    );
    harness
        .prompt(
            &session.id,
            "first",
            RunOptions {
                model: Some(CHAT_MODEL.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let result = harness
        .prompt(
            &session.id,
            "read a",
            RunOptions {
                model: Some(CHAT_MODEL.into()),
                max_tool_calls: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.text.as_deref(), Some("done"));
    assert_eq!(tools.0.load(Ordering::SeqCst), 1);
    let history = store.messages(&session.id).unwrap();
    let final_state = history.last().unwrap().provider_state.as_ref().unwrap();
    assert_eq!(
        final_state["message_fields"]["reasoning_content"],
        "R-final"
    );
    let all_requests = mock.requests.lock().unwrap();
    let requests = &all_requests[1..];
    assert_eq!(requests.len(), 2);
    assert!(requests[0].get("tools").is_some());
    assert_eq!(requests[1]["tools"], requests[0]["tools"]);
    assert!(requests[1].get("tool_choice").is_none());
    let messages = requests[1]["messages"].as_array().unwrap();
    assert_eq!(
        messages[2],
        json!({"role":"assistant","content":"Answer one"})
    );
    // The run's budget instruction is stored once in history before the tool loop.
    assert_eq!(messages[4]["role"], "user");
    assert!(
        messages[4]["content"]
            .as_str()
            .unwrap()
            .starts_with("<harness kind=\"tool_budget:")
    );
    assert_eq!(messages[5]["reasoning_content"], "R-loop");
    assert_eq!(messages[5]["tool_calls"][0]["id"], "call-1");
    assert_eq!(messages[6]["role"], "tool");
    assert!(
        messages[6]["content"]
            .as_str()
            .unwrap()
            .starts_with("file a")
    );
    assert_eq!(messages[6]["tool_call_id"], "call-1");
    assert_eq!(requests[1].to_string().matches("R-loop").count(), 1);
    assert!(!requests[1].to_string().contains("R-closed"));
}

// Cross-provider: foreign reasoning stays stored and returns to its origin.

const RESPONSES_MODEL: &str = "gpt-6-luna";
const MESSAGES_MODEL: &str = "claude-haiku-5-5";

fn responses_reply(reasoning: &Value, text: &str) -> String {
    let message = json!({"type":"message","id":"msg-r","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]});
    sse(&[
        json!({"type":"response.output_text.delta","delta":text}),
        json!({"type":"response.completed","response":{"status":"completed","output":[reasoning, message],"usage":{"input_tokens":6,"output_tokens":4}}}),
    ])
}

fn messages_reply(thinking: &Value, text: &str) -> String {
    let mut start = thinking.clone();
    start["thinking"] = json!("");
    start["signature"] = json!("");
    sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":start}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":thinking["thinking"]}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":thinking["signature"]}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":text}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ])
}

#[tokio::test]
async fn foreign_reasoning_is_kept_and_replayed_when_the_session_returns_to_its_origin() {
    let reasoning = json!({"type":"reasoning","id":"rs-origin","encrypted_content":"enc-origin+/==猫","summary":[]});
    let thinking = json!({"type":"thinking","thinking":"Plan","signature":"sig-origin+/==猫"});
    let (base, mock) = start(vec![
        responses_reply(&reasoning, "R one"),
        messages_reply(&thinking, "M two"),
        responses_reply(
            &json!({"type":"reasoning","id":"rs-3","encrypted_content":"enc-3","summary":[]}),
            "R three",
        ),
        messages_reply(
            &json!({"type":"thinking","thinking":"x","signature":"sig-4"}),
            "M four",
        ),
    ])
    .await;
    let responses = ResponsesProvider::api_key("synthetic-key", base.clone()).unwrap();
    let messages = MessagesProvider::api_key("synthetic-key", base).unwrap();
    let mut history = vec![message(Role::User, "one")];
    let first = responses
        .generate(
            request(RESPONSES_MODEL, history.clone()),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    history.push(assistant(&first.text, first.provider_state));
    history.push(message(Role::User, "two"));
    let second = messages
        .generate(
            request(MESSAGES_MODEL, history.clone()),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    history.push(assistant(&second.text, second.provider_state));
    history.push(message(Role::User, "three"));
    let stored = serde_json::to_string(&history).unwrap();
    assert!(stored.contains("enc-origin+/==猫") && stored.contains("sig-origin+/==猫"));
    responses
        .generate(
            request(RESPONSES_MODEL, history.clone()),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    messages
        .generate(
            request(MESSAGES_MODEL, history),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let requests = mock.requests.lock().unwrap();
    // While on Messages, the Responses reasoning is omitted.
    assert!(!requests[1].to_string().contains("enc-origin"));
    // Back on Responses: its own item returns unchanged; the Messages signature does not.
    assert_eq!(requests[2]["input"][1], reasoning);
    assert!(!requests[2].to_string().contains("sig-origin"));
    // Back on Messages: its own thinking block returns unchanged.
    assert_eq!(requests[3]["messages"][3]["content"][0], thinking);
    assert!(!requests[3].to_string().contains("enc-origin"));
}

#[tokio::test]
async fn chat_all_replay_sends_reasoning_only_to_its_origin_provider_and_model() {
    let reasoning = json!({"type":"reasoning","id":"rs-foreign-chat","encrypted_content":"enc-foreign-chat+/==猫","summary":[]});
    let (base, mock) = start(vec![
        responses_reply(&reasoning, "foreign answer"),
        chat_reply("R-chat-origin+/==猫", "chat answer"),
        chat_reply("R-mismatch", "other answer"),
        chat_reply("R-return", "origin answer"),
        responses_reply(&json!({"type":"reasoning","id":"rs-return","encrypted_content":"enc-return","summary":[]}), "back"),
    ]).await;
    let responses = ResponsesProvider::api_key("synthetic-key", base.clone()).unwrap();
    let chat = ChatCompletionsProvider::api_key("synthetic-key", base)
        .unwrap()
        .with_reasoning_replay(BTreeMap::from([
            (CHAT_MODEL.into(), ReasoningReplay::All),
            ("other-chat-model".into(), ReasoningReplay::All),
        ]))
        .unwrap();
    let mut history = vec![message(Role::User, "one")];
    let first = responses
        .generate(
            request(RESPONSES_MODEL, history.clone()),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    history.push(assistant(&first.text, first.provider_state));
    history.push(message(Role::User, "two"));
    let second = chat
        .generate(
            request(CHAT_MODEL, history.clone()),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    history.push(assistant(&second.text, second.provider_state));
    history.push(message(Role::User, "three"));
    let stored = serde_json::to_vec(&history).unwrap();
    chat.generate(
        request("other-chat-model", history.clone()),
        noop(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    chat.generate(
        request(CHAT_MODEL, history.clone()),
        noop(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    responses
        .generate(
            request(RESPONSES_MODEL, history.clone()),
            noop(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&history).unwrap(),
        stored,
        "wire filtering keeps canonical data"
    );
    let requests = mock.requests.lock().unwrap();
    assert!(!requests[1].to_string().contains("enc-foreign-chat"));
    assert!(!requests[2].to_string().contains("R-chat-origin"));
    assert!(!requests[2].to_string().contains("enc-foreign-chat"));
    assert_eq!(
        requests[3]["messages"][4]["reasoning_content"],
        "R-chat-origin+/==猫"
    );
    assert!(!requests[3].to_string().contains("enc-foreign-chat"));
    assert_eq!(requests[4]["input"][1], reasoning);
    assert!(!requests[4].to_string().contains("R-chat-origin"));
}
