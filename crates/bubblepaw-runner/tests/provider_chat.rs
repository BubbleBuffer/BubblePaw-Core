use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use bubblepaw_core::{
    CancellationToken, JsonSchema, Message, ModelRequest, Provider, ProviderError,
    ProviderErrorKind, ReasoningEffort, ReasoningReplay, Role, Tool, ToolCall, ToolContext,
    ToolDefinition, ToolRegistry, ToolResult,
};
use bubblepaw_runner::provider::{
    ChatCompletionsProvider, ProviderRegistry, RetryPolicy, decision_error_usage,
    provider_retry_after,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[path = "support/worker_jobs.rs"]
mod worker_jobs;

type Captured = Arc<Mutex<Vec<(HeaderMap, Vec<u8>)>>>;
type Reply = (StatusCode, &'static str, String);
type Replies = Arc<Mutex<VecDeque<Reply>>>;
async fn capture_server() -> (String, Captured, tokio::task::JoinHandle<()>) {
    let captured: Captured = Arc::default();
    let app = Router::new().route("/chat/completions", post(|State(captured): State<Captured>, headers: HeaderMap, bytes: Bytes| async move {
        captured.lock().unwrap().push((headers, bytes.to_vec()));
        axum::Json(json!({"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}))
    })).with_state(captured.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, captured, server)
}

#[derive(Clone)]
struct Fixture {
    requests: Captured,
    replies: Replies,
}
async fn serve(
    replies: Vec<(StatusCode, &'static str, String)>,
) -> (String, Captured, tokio::task::JoinHandle<()>) {
    let fixture = Fixture {
        requests: Arc::default(),
        replies: Arc::new(Mutex::new(replies.into())),
    };
    let app = Router::new()
        .route(
            "/chat/completions",
            post(
                |State(fixture): State<Fixture>, headers: HeaderMap, bytes: Bytes| async move {
                    fixture
                        .requests
                        .lock()
                        .unwrap()
                        .push((headers, bytes.to_vec()));
                    let (status, mime, body) = fixture
                        .replies
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("unexpected retry");
                    let delay = if status == StatusCode::TOO_MANY_REQUESTS {
                        "3"
                    } else {
                        "0"
                    };
                    (
                        status,
                        [("content-type", mime), ("retry-after", delay)],
                        body,
                    )
                        .into_response()
                },
            ),
        )
        .with_state(fixture.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, fixture.requests, server)
}
fn reply(message: Value, finish: &str) -> (StatusCode, &'static str, String) {
    (StatusCode::OK, "application/json", json!({"choices":[{"message":message,"finish_reason":finish}],"usage":{"prompt_tokens":10,"completion_tokens":5}}).to_string())
}
fn sse(events: &[Value]) -> (StatusCode, &'static str, String) {
    let mut body: String = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
    body.push_str("data: [DONE]\n\n");
    (StatusCode::OK, "text/event-stream", body)
}
fn tool_request() -> ModelRequest {
    let mut req = request();
    req.tools = vec![ToolDefinition {
        name: "read".into(),
        description: "Read a file".into(),
        input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        read_only: true,
        approval: Default::default(),
    }];
    req
}
fn wire_call(id: &str, name: &str, arguments: &str) -> Value {
    json!({"id":id,"type":"function","function":{"name":name,"arguments":arguments}})
}
fn body(requests: &Captured, index: usize) -> Value {
    serde_json::from_slice(&requests.lock().unwrap()[index].1).unwrap()
}

// https://openrouter.ai/docs/guides/best-practices/reasoning-tokens:
// delta.reasoning_details is an ordered sequence across chunks, even when
// objects share an index. Plain reasoning can mirror those same text bytes.
fn openrouter_details() -> Vec<Value> {
    vec![
        json!({"type":"reasoning.text","text":"Think ","signature":null,"id":"reasoning-text-1","format":"anthropic-claude-v1","index":0}),
        json!({"type":"reasoning.text","text":"π","signature":"sig+/==\n猫","id":"reasoning-text-1","format":"anthropic-claude-v1","index":0}),
        json!({"type":"reasoning.summary","summary":"plan","id":null,"format":"unknown","index":1}),
        json!({"type":"reasoning.encrypted","data":"cipher+/==\n猫","id":"encrypted-1","format":"openai-responses-v1","index":2}),
    ]
}

#[tokio::test]
async fn openrouter_reasoning_continuation_is_stored_and_replayed_in_order() {
    let details = openrouter_details();
    let (base, requests, server) = serve(vec![
        sse(&[
            json!({"choices":[{"delta":{"reasoning":"Think ","reasoning_details":[details[0].clone()]}}]}),
            json!({"choices":[{"delta":{"reasoning":"π","reasoning_details":details[1..].to_vec(),"content":"A"},"finish_reason":"length"}]}),
        ]),
        sse(&[json!({"choices":[{"delta":{"reasoning":"tail","reasoning_details":[details[3].clone()],"content":"B"},"finish_reason":"stop"}]})]),
        reply(json!({"content":"next"}), "stop"),
        reply(json!({"content":"foreign"}), "stop"),
    ]).await;
    let provider = ChatCompletionsProvider::api_key("synthetic-key", base)
        .unwrap()
        .with_reasoning_replay(BTreeMap::from([(
            "chat-model".into(),
            ReasoningReplay::All,
        )]))
        .unwrap();
    let response = provider
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.text, "AB");
    let state = response.provider_state.clone().unwrap();
    assert_eq!(state["rounds"][0]["message_fields"]["reasoning"], "Think π");
    assert_eq!(
        state["rounds"][0]["message_fields"]["reasoning_details"],
        json!(details)
    );
    assert_eq!(state["rounds"][1]["message_fields"]["reasoning"], "tail");
    assert_eq!(
        state["rounds"][1]["message_fields"]["reasoning_details"],
        json!([details[3].clone()])
    );
    for round in state["rounds"].as_array().unwrap() {
        assert!(
            round.get("opaque_stream_fragments").is_none(),
            "assembled reasoning must not be duplicated in the journal"
        );
    }
    assert_eq!(body(&requests, 1)["messages"][2]["reasoning"], "Think π");
    assert_eq!(
        body(&requests, 1)["messages"][2]["reasoning_details"],
        json!(details)
    );

    let root = tempfile::tempdir().unwrap();
    let store = bubblepaw_core::Store::open(root.path().join("chat.sqlite")).unwrap();
    let session = store.create_session("OpenRouter continuation").unwrap();
    let mut assistant = message(Role::Assistant, &response.text);
    assistant.session_id = session.id.clone();
    assistant.provider_state = Some(state.clone());
    store.append_batch(&[assistant]).unwrap();
    let stored = store.messages(&session.id).unwrap();
    assert_eq!(
        serde_json::to_vec(stored[0].provider_state.as_ref().unwrap()).unwrap(),
        serde_json::to_vec(&state).unwrap()
    );
    let mut next = request();
    next.messages.extend(stored);
    provider
        .generate(next.clone(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    let sent = body(&requests, 2);
    assert_eq!(sent["messages"][2]["content"], "A");
    assert_eq!(sent["messages"][2]["reasoning"], "Think π");
    assert_eq!(sent["messages"][2]["reasoning_details"], json!(details));
    assert_eq!(sent["messages"][3]["content"], "B");
    assert_eq!(sent["messages"][3]["reasoning"], "tail");
    assert_eq!(
        sent["messages"][3]["reasoning_details"],
        json!([details[3].clone()])
    );
    next.model = "foreign-model".into();
    provider
        .generate(next, Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    let foreign = body(&requests, 3);
    assert_eq!(
        foreign["messages"][2],
        json!({"role":"assistant","content":"AB"})
    );
    server.abort();
}

#[tokio::test]
async fn openrouter_reasoning_tool_loop_replays_plain_details_or_both() {
    for (plain, detailed) in [(true, false), (false, true), (true, true)] {
        let details = openrouter_details();
        let mut delta = json!({"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}}]});
        if plain {
            delta["reasoning"] = json!("Think π");
        }
        if detailed {
            delta["reasoning_details"] = json!(details);
        }
        let (base, requests, server) = serve(vec![
            sse(&[json!({"choices":[{"delta":delta,"finish_reason":"tool_calls"}]})]),
            reply(json!({"content":"Done."}), "stop"),
        ])
        .await;
        let provider = ChatCompletionsProvider::api_key("synthetic-key", base).unwrap();
        let mut req = tool_request();
        let first = provider
            .generate(req.clone(), Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap();
        let mut assistant = message(Role::Assistant, &first.text);
        assistant.provider_state = first.provider_state;
        assistant.tool_calls = first.tool_calls;
        req.messages.push(assistant);
        let mut result = message(Role::Tool, "file contents");
        result.tool_call_id = Some("call_a".into());
        req.messages.push(result);
        provider
            .generate(req, Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap();
        let sent = body(&requests, 1);
        assert_eq!(
            sent["messages"][2]["reasoning"],
            if plain {
                json!("Think π")
            } else {
                Value::Null
            }
        );
        assert_eq!(
            sent["messages"][2]["reasoning_details"],
            if detailed {
                json!(details)
            } else {
                Value::Null
            }
        );
        assert_eq!(sent["messages"][2]["tool_calls"][0]["id"], "call_a");
        server.abort();
    }
}

#[tokio::test]
async fn fragmented_single_and_parallel_calls_have_native_result_round_trips() {
    for parallel in [false, true] {
        let mut events = vec![
            json!({"choices":[{"delta":{"reasoning_content":"Exact thought. ","tool_calls":[{"index":0,"id":"call_","type":"function","function":{"name":"re","arguments":"{\"path\":"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"ad","arguments":"\"a\"}"}}]}}]}),
        ];
        if parallel {
            events.insert(1, json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"read","arguments":"{\"path\":\""}}]}}]}));
            events.push(json!({"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"b\"}"}}]}}]}));
        }
        events.extend([
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":15,"completion_tokens":8,"completion_tokens_details":{"reasoning_tokens":6}}}),
        ]);
        let (base, requests, server) = serve(vec![
            sse(&events),
            reply(json!({"content":"Done."}), "stop"),
        ])
        .await;
        let provider = ChatCompletionsProvider::api_key("synthetic-key", base)
            .unwrap()
            .with_reasoning_replay(BTreeMap::from([(
                "chat-model".into(),
                ReasoningReplay::ToolLoop,
            )]))
            .unwrap();
        let mut req = tool_request();
        req.reasoning_effort = Some(ReasoningEffort::High);
        let first = provider
            .generate(
                req.clone(),
                Arc::new(|_| panic!("tool-only text published")),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(first.tool_calls.len(), if parallel { 2 } else { 1 });
        assert_eq!(first.tool_calls[0].id, "call_a");
        assert_eq!(first.tool_calls[0].arguments, json!({"path":"a"}));
        assert_eq!(first.usage.as_ref().unwrap().reasoning_tokens, Some(6));
        assert_eq!(
            first.provider_state.as_ref().unwrap()["message_fields"]["reasoning_content"],
            "Exact thought. "
        );
        assert!(
            first.provider_state.as_ref().unwrap()["message_fields"]
                .get("tool_calls")
                .is_none()
        );
        let mut assistant = message(Role::Assistant, "");
        assistant.tool_calls = first.tool_calls.clone();
        assistant.provider_state = first.provider_state;
        req.messages.push(assistant);
        for call in &first.tool_calls {
            let mut result = message(Role::Tool, "file contents");
            result.tool_call_id = Some(call.id.clone());
            req.messages.push(result);
        }
        req.disabled_tools = std::mem::take(&mut req.tools); // Tools off keeps the declarations.
        assert_eq!(
            provider
                .generate(req, Arc::new(|_| {}), CancellationToken::new())
                .await
                .unwrap()
                .text,
            "Done."
        );
        let sent = body(&requests, 0);
        assert_eq!(
            sent["tools"][0],
            json!({"type":"function","function":{"name":"read","description":"Read a file","parameters":tool_request().tools[0].input_schema}})
        );
        assert!(sent.get("tool_choice").is_none());
        assert!(sent.get("parallel_tool_calls").is_none());
        assert_eq!(sent["reasoning_effort"], "high");
        assert_eq!(
            requests.lock().unwrap()[0].0["authorization"],
            "Bearer synthetic-key"
        );
        let second = body(&requests, 1);
        assert_eq!(second["tools"], sent["tools"]);
        assert_eq!(second["tool_choice"], "none");
        assert_eq!(
            second["messages"][2]["reasoning_content"],
            "Exact thought. "
        );
        assert_eq!(second["messages"][2]["content"], Value::Null);
        assert_eq!(
            second["messages"][2]["tool_calls"][0],
            wire_call("call_a", "read", r#"{"path":"a"}"#)
        );
        assert_eq!(
            second["messages"][3],
            json!({"role":"tool","content":"file contents","tool_call_id":"call_a"})
        );
        if parallel {
            assert_eq!(
                second["messages"][2]["tool_calls"][1],
                wire_call("call_b", "read", r#"{"path":"b"}"#)
            );
            assert_eq!(second["messages"][4]["tool_call_id"], "call_b");
        }
        server.abort();
    }
}

#[derive(Default)]
struct CountingTools(AtomicUsize);
#[async_trait::async_trait]
impl Tool for CountingTools {
    fn definition(&self) -> ToolDefinition {
        tool_request().tools.remove(0)
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> anyhow::Result<ToolResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text("executed"))
    }
}
#[tokio::test]
async fn malformed_calls_are_metered_and_never_executed_by_the_host() {
    for calls in [
        json!([wire_call("a", "read", "{bad")]),
        json!([wire_call("a", "read", "[]")]),
        json!([wire_call("a", "read", "{}"), wire_call("a", "read", "{}")]),
        json!([wire_call("a", "unavailable", "{}")]),
        json!([wire_call("", "read", "{}")]),
        json!("not an array"),
    ] {
        let malformed = reply(
            json!({"content":"must not publish","tool_calls":calls}),
            "tool_calls",
        );
        let (base, requests, server) = serve(vec![malformed.clone(), malformed]).await;
        let provider = ChatCompletionsProvider::api_key("synthetic-key", base).unwrap();
        let error = provider
            .generate(
                tool_request(),
                Arc::new(|_| panic!("malformed reply text published")),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            ProviderError::of(&error).unwrap().kind,
            ProviderErrorKind::Fatal
        );
        assert_eq!(decision_error_usage(&error).unwrap().input_tokens, 10);
        let tools = Arc::new(CountingTools::default());
        let mut registry = ToolRegistry::new();
        registry.register(tools.clone()).unwrap();
        let fixture =
            worker_jobs::Fixture::new(Arc::new(provider), registry, &tool_request().model);
        let run = fixture.run("Read a file.").await;
        assert_eq!(run.status, "failed");
        assert_eq!(
            run.usage.as_ref().map(|usage| usage.input_tokens),
            Some(10),
            "run={run:?}; requests={}",
            requests.lock().unwrap().len()
        );
        assert_eq!(run.usage.as_ref().unwrap().output_tokens, 5);
        assert_eq!(tools.0.load(Ordering::SeqCst), 0);
        assert_eq!(requests.lock().unwrap().len(), 2);
        server.abort();
    }
}

#[tokio::test]
async fn malformed_stream_arguments_keep_usage_and_a_typed_error() {
    let events = [
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"read","arguments":"{bad"}}]},"finish_reason":"tool_calls"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":4,"cache_creation_tokens":2},"completion_tokens_details":{"reasoning_tokens":1}}}),
    ];
    let (base, _, server) = serve(vec![sse(&events)]).await;
    let error = ChatCompletionsProvider::api_key("key", base)
        .unwrap()
        .generate(
            tool_request(),
            Arc::new(|_| panic!("no text")),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        ProviderError::of(&error).unwrap().kind,
        ProviderErrorKind::Fatal
    );
    assert_eq!(decision_error_usage(&error).unwrap().input_tokens, 11);
    let usage = ProviderError::of(&error).unwrap().usage.as_ref().unwrap();
    assert_eq!(usage.input_tokens, 11);
    assert_eq!(usage.output_tokens, 3);
    assert_eq!(usage.cache_read_tokens, 4);
    assert_eq!(usage.cache_creation_tokens, 2);
    assert_eq!(usage.reasoning_tokens, Some(1));
    server.abort();
}

#[tokio::test]
async fn native_error_finish_is_fatal_metered_and_never_an_empty_success() {
    for streamed in [true, false] {
        for text in ["", "partial visible text"] {
            let usage = json!({"prompt_tokens":0,"completion_tokens":0,"total_tokens":0});
            let terminal = json!({"choices":[{"index":0,"delta":{"content":""},"finish_reason":"error","native_finish_reason":"MALFORMED_FUNCTION_CALL"}],"usage":usage});
            let response = if streamed {
                let mut events = Vec::new();
                if !text.is_empty() {
                    events.push(json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}));
                }
                events.push(terminal);
                sse(&events)
            } else {
                (StatusCode::OK, "application/json", json!({"choices":[{"index":0,"message":{"content":text},"finish_reason":"error","native_finish_reason":"MALFORMED_FUNCTION_CALL"}],"usage":usage}).to_string())
            };
            let (base, requests, server) = serve(vec![response.clone(), response]).await;
            let provider = ChatCompletionsProvider::api_key("synthetic-key", base).unwrap();
            let published = Arc::new(Mutex::new(String::new()));
            let sink = published.clone();
            let error = provider
                .generate(
                    request(),
                    Arc::new(move |text| sink.lock().unwrap().push_str(text)),
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            let classified = ProviderError::of(&error).unwrap();
            assert_eq!(classified.kind, ProviderErrorKind::Fatal);
            assert_eq!(classified.published, streamed && !text.is_empty());
            assert!(!bubblepaw_core::provider_error_retryable(&error));
            assert_eq!(
                classified.usage.as_ref(),
                Some(&bubblepaw_core::Usage::default())
            );
            assert_eq!(decision_error_usage(&error).unwrap().input_tokens, 0);
            assert_eq!(
                error.to_string(),
                "Chat completions response ended with error (MALFORMED_FUNCTION_CALL)"
            );
            assert_eq!(*published.lock().unwrap(), if streamed { text } else { "" });

            let fixture = worker_jobs::Fixture::new(
                Arc::new(provider),
                ToolRegistry::new(),
                &request().model,
            );
            let run = fixture.run("Answer.").await;
            assert_eq!(run.status, "failed");
            assert_eq!(run.usage, Some(bubblepaw_core::Usage::default()));
            assert!(run.error.unwrap().contains("MALFORMED_FUNCTION_CALL"));
            assert_eq!(requests.lock().unwrap().len(), 2);
            server.abort();
        }
    }
}

#[tokio::test]
async fn recorded_flash_lite_native_error_is_a_failed_run() {
    // Recorded OpenRouter Gemini Flash Lite response from the image acceptance
    // run. Includes reasoning-only chunks, processing comments and zero usage.
    let raw = include_str!("fixtures/live_e2e_flash_lite_error.sse");
    let response = (StatusCode::OK, "text/event-stream", raw.to_owned());
    let (base, requests, server) = serve(vec![response.clone(), response]).await;
    let provider = ChatCompletionsProvider::api_key("synthetic-key", base).unwrap();
    let error = provider
        .generate(
            request(),
            Arc::new(|_| panic!("reasoning-only failed reply published text")),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    let classified = ProviderError::of(&error).unwrap();
    assert_eq!(classified.kind, ProviderErrorKind::Fatal);
    assert!(!classified.published);
    assert!(!bubblepaw_core::provider_error_retryable(&error));
    let expected = bubblepaw_core::Usage {
        reasoning_tokens: Some(0),
        ..Default::default()
    };
    assert_eq!(classified.usage.as_ref(), Some(&expected));
    assert!(error.to_string().contains("MALFORMED_FUNCTION_CALL"));

    let fixture =
        worker_jobs::Fixture::new(Arc::new(provider), ToolRegistry::new(), &request().model);
    let run = fixture.run("Inspect the fixture image.").await;
    assert_eq!(run.status, "failed");
    assert_eq!(run.usage, Some(expected));
    assert!(run.error.unwrap().contains("MALFORMED_FUNCTION_CALL"));
    assert_eq!(requests.lock().unwrap().len(), 2);
    server.abort();
}

#[tokio::test]
async fn http_errors_are_typed_redacted_and_keep_rate_limit_metadata() {
    for (status, payload, expected) in [
        (
            401,
            json!({"error":{"message":"private credentials"}}),
            ProviderErrorKind::Auth,
        ),
        (
            403,
            json!({"error":{"message":"private credentials"}}),
            ProviderErrorKind::Auth,
        ),
        (
            400,
            json!({"error":{"code":"context_length_exceeded","message":"private prompt"}}),
            ProviderErrorKind::Context,
        ),
        (
            400,
            json!({"error":{"message":"maximum context length exceeded private prompt"}}),
            ProviderErrorKind::Context,
        ),
        (
            503,
            json!({"error":{"message":"private prompt"}}),
            ProviderErrorKind::Transient,
        ),
        (
            400,
            json!({"error":{"message":"private invalid request"}}),
            ProviderErrorKind::Fatal,
        ),
        (
            429,
            json!({"error":{"message":"private quota"}}),
            ProviderErrorKind::RateLimit { resets_at: None },
        ),
    ] {
        let (base, requests, server) = serve(vec![(
            StatusCode::from_u16(status).unwrap(),
            "application/json",
            payload.to_string(),
        )])
        .await;
        let before = chrono::Utc::now();
        let error = ChatCompletionsProvider::api_key("synthetic-key", base)
            .unwrap()
            .generate(request(), Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap_err();
        let after = chrono::Utc::now();
        let kind = &ProviderError::of(&error).unwrap().kind;
        if status == 429 {
            let ProviderErrorKind::RateLimit {
                resets_at: Some(reset),
            } = kind
            else {
                panic!("missing reset {kind:?}")
            };
            let reset = chrono::DateTime::parse_from_rfc3339(reset).unwrap();
            assert!(reset >= before + chrono::Duration::seconds(3));
            assert!(reset <= after + chrono::Duration::seconds(3));
            let delay = provider_retry_after(&error).unwrap();
            assert!(delay > Duration::from_secs(2) && delay <= Duration::from_secs(3));
        } else {
            assert_eq!(*kind, expected);
        }
        let diagnostic = error.to_string();
        assert!(diagnostic.starts_with(&format!("Chat completions HTTP {status}: ")));
        assert!(diagnostic.contains(payload["error"]["message"].as_str().unwrap()));
        assert_eq!(requests.lock().unwrap().len(), 1);
        server.abort();
    }
}

#[tokio::test]
async fn worker_preserves_safe_request_retries_but_never_replays_published_streams() {
    let error = json!({"error":{"type":"server_error","message":"private server diagnostic"}});
    for (published, replies) in [
        (
            false,
            vec![
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "application/json",
                    "private".into(),
                ),
                sse(std::slice::from_ref(&error)),
                reply(json!({"content":"recovered"}), "stop"),
            ],
        ),
        (
            true,
            vec![sse(&[
                json!({"choices":[{"delta":{"content":"published"}}]}),
                error,
            ])],
        ),
    ] {
        let (base, requests, server) = serve(replies).await;
        let provider = ChatCompletionsProvider::api_key("key", base).unwrap();
        // Request retries belong to the registry's one policy for main turns.
        let registry =
            ProviderRegistry::new("chat", Arc::new(provider)).with_retry_policy(RetryPolicy {
                initial_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
                ..RetryPolicy::default()
            });
        let fixture =
            worker_jobs::Fixture::new(Arc::new(registry), ToolRegistry::new(), &request().model);
        let run = fixture.run("Hello").await;
        if published {
            assert_eq!(run.status, "failed");
            assert_eq!(requests.lock().unwrap().len(), 1);
        } else {
            assert_eq!(run.text.as_deref(), Some("recovered"));
            assert_eq!(requests.lock().unwrap().len(), 3);
            assert_eq!(body(&requests, 0), body(&requests, 1));
            assert_eq!(body(&requests, 1), body(&requests, 2));
        }
        server.abort();
    }
}

#[tokio::test]
async fn typed_stream_error_chunks_are_redacted() {
    for (kind, expected) in [
        ("authentication_error", ProviderErrorKind::Auth),
        (
            "rate_limit_error",
            ProviderErrorKind::RateLimit { resets_at: None },
        ),
        ("context_length_exceeded", ProviderErrorKind::Context),
        ("server_error", ProviderErrorKind::Transient),
    ] {
        let (base, _, server) = serve(vec![sse(&[
            json!({"error":{"type":kind,"message":"private diagnostic"}}),
        ])])
        .await;
        let error = ChatCompletionsProvider::api_key("key", base)
            .unwrap()
            .generate(request(), Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(ProviderError::of(&error).unwrap().kind, expected);
        assert!(!ProviderError::of(&error).unwrap().published);
        assert_eq!(
            error.to_string(),
            "Chat completions stream reported an error"
        );
        server.abort();
    }
}

#[tokio::test]
async fn chat_failure_keeps_publication_across_continuation_rounds() {
    for continued in [false, true] {
        let mut replies = Vec::new();
        if continued {
            replies.push(reply(json!({"content":"published"}), "length"));
            replies.push((
                StatusCode::SERVICE_UNAVAILABLE,
                "application/json",
                "private".into(),
            ));
        } else {
            replies.push(sse(&[
                json!({"choices":[{"delta":{"content":"published"}}]}),
                json!({"error":{"type":"server_error"}}),
            ]));
        }
        let (base, requests, server) = serve(replies.clone()).await;
        let published = Arc::new(Mutex::new(String::new()));
        let sink = published.clone();
        let error = ChatCompletionsProvider::api_key("key", base)
            .unwrap()
            .generate(
                request(),
                Arc::new(move |text| sink.lock().unwrap().push_str(text)),
                CancellationToken::new(),
            )
            .await
            .unwrap_err()
            .context("host context");
        assert_eq!(*published.lock().unwrap(), "published");
        assert!(ProviderError::of(&error).unwrap().published);
        assert!(!bubblepaw_core::provider_error_retryable(&error));
        if continued {
            assert_eq!(
                ProviderError::of(&error).unwrap().kind,
                ProviderErrorKind::Transient
            );
            assert!(provider_retry_after(&error).is_some());
        }
        assert_eq!(
            requests.lock().unwrap().len(),
            if continued { 2 } else { 1 }
        );
        server.abort();

        // Exercise an ordinary harness run too: a continuation failure must not
        // replay the already-published first round.
        let (base, requests, server) = serve(replies).await;
        let provider = ChatCompletionsProvider::api_key("key", base).unwrap();
        let fixture =
            worker_jobs::Fixture::new(Arc::new(provider), ToolRegistry::new(), &request().model);
        let run = fixture.run("Hello").await;
        assert_eq!(run.status, "failed");
        assert_eq!(
            requests.lock().unwrap().len(),
            if continued { 2 } else { 1 }
        );
        server.abort();
    }
}

#[tokio::test]
async fn deepseek_usage_fallback_and_config_effort_are_preserved() {
    let payload = json!({"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_cache_hit_tokens":20,"prompt_cache_miss_tokens":7,"completion_tokens":5,"cache_creation_input_tokens":3,"completion_tokens_details":{"reasoning_tokens":2}}});
    let (base, requests, server) = serve(vec![(
        StatusCode::OK,
        "application/json",
        payload.to_string(),
    )])
    .await;
    let provider = ChatCompletionsProvider::api_key("key", base)
        .unwrap()
        .with_reasoning_fields(json!({"thinking":{"type":"enabled"},"reasoning_effort":"medium"}))
        .unwrap();
    let response = provider
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    let usage = response.usage.unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_creation_tokens,
            usage.reasoning_tokens
        ),
        (27, 5, 20, 3, Some(2))
    );
    assert_eq!(body(&requests, 0)["reasoning_effort"], "medium");
    assert_eq!(body(&requests, 0)["thinking"], json!({"type":"enabled"}));
    server.abort();
}

#[tokio::test]
async fn invalid_tool_history_never_reaches_http() {
    let (base, requests, server) = capture_server().await;
    let provider = ChatCompletionsProvider::api_key("key", base).unwrap();
    let mut history = request();
    history
        .messages
        .push(message(Role::Tool, "orphaned result"));
    let error = provider
        .generate(history, Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        ProviderError::of(&error).unwrap().kind,
        ProviderErrorKind::Fatal
    );
    assert!(error.to_string().contains("tool_call_id"));
    assert!(requests.lock().unwrap().is_empty());
    server.abort();
}

#[test]
fn missing_and_malformed_credentials_and_unsafe_urls_fail_fast() {
    for key in ["", "   ", "bad\nheader", "bad\rheader"] {
        assert!(ChatCompletionsProvider::api_key(key, "https://example.com/v1").is_err());
    }
    for base in [
        "file:///private",
        "http://user:password@example.com",
        "https://example.com/v1?token=private",
        "https://example.com/v1#fragment",
    ] {
        assert!(
            ChatCompletionsProvider::api_key("key", base).is_err(),
            "{base}"
        );
    }
}

#[tokio::test]
async fn a_reported_zero_reasoning_token_count_remains_known() {
    let events = [
        json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"completion_tokens_details":{"reasoning_tokens":0}}}),
    ];
    let (base, _, server) = serve(vec![sse(&events)]).await;
    let response = ChatCompletionsProvider::api_key("key", base)
        .unwrap()
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.usage.unwrap().reasoning_tokens, Some(0));
    server.abort();
}

#[tokio::test]
async fn connection_failure_is_a_redacted_transient_error() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let error = ChatCompletionsProvider::api_key("key", base)
        .unwrap()
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        ProviderError::of(&error).unwrap().kind,
        ProviderErrorKind::Transient
    );
    assert_eq!(error.to_string(), "Chat completions transport failed");
}

#[tokio::test]
async fn empty_replay_rounds_keep_canonical_calls_on_the_assistant() {
    let (base, requests, server) = capture_server().await;
    let provider = ChatCompletionsProvider::api_key("key", base)
        .unwrap()
        .with_reasoning_replay(BTreeMap::from([(
            "chat-model".into(),
            ReasoningReplay::All,
        )]))
        .unwrap();
    let mut req = request();
    let mut assistant = message(Role::Assistant, "");
    assistant.tool_calls = vec![ToolCall {
        id: "call_a".into(),
        name: "read".into(),
        arguments: json!({"path":"a"}),
    }];
    assistant.provider_state = Some(
        bubblepaw_core::with_provider_state_origin(
            json!({"adapter":"chat-completions","rounds":[]}),
            "chat-completions",
            Some("chat-model"),
            "native",
        )
        .unwrap(),
    );
    req.messages.push(assistant);
    let mut result = message(Role::Tool, "result");
    result.tool_call_id = Some("call_a".into());
    req.messages.push(result);
    provider
        .generate(req, Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    let sent = body(&requests, 0);
    assert_eq!(
        sent["messages"][1],
        json!({"role":"user","content":"Hello."})
    );
    assert_eq!(
        sent["messages"][2],
        json!({"role":"assistant","content":null,"tool_calls":[wire_call("call_a","read",r#"{"path":"a"}"#)]})
    );
    assert_eq!(
        sent["messages"][3],
        json!({"role":"tool","content":"result","tool_call_id":"call_a"})
    );
    assert!(sent.get("tools").is_none());
    server.abort();
}
fn message(role: Role, content: &str) -> Message {
    Message {
        id: "message".into(),
        session_id: "session".into(),
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
fn request() -> ModelRequest {
    ModelRequest {
        model: "chat-model".into(),
        system_prompt: "System.".into(),
        messages: vec![message(Role::User, "Hello.")],
        ..Default::default()
    }
}

// Freeze the actual HTTP body bytes before the adapter gains tools. These literals
// deliberately do not pass through JSON serialization or Value comparison.
#[tokio::test]
async fn no_tools_request_body_bytes_are_frozen() {
    let (base, captured, server) = capture_server().await;
    let ordinary = ChatCompletionsProvider::api_key("synthetic-key", &base).unwrap();
    let replay = ChatCompletionsProvider::api_key("synthetic-key", base)
        .unwrap()
        .with_reasoning_replay(BTreeMap::from([(
            "chat-model".into(),
            ReasoningReplay::All,
        )]))
        .unwrap();
    let mut history = request();
    let mut assistant = message(Role::Assistant, "Prior.");
    assistant.provider_state = Some(json!({"reasoning_content":"Exact reasoning."}));
    history.messages.push(assistant);
    let mut capped = request();
    capped.max_output_tokens = Some(42);
    let mut structured = request();
    structured.output_schema = Some(JsonSchema::new("answer", json!({"type":"object","additionalProperties":false,"properties":{"answer":{"type":"string"}},"required":["answer"]})).unwrap());
    for (provider, req) in [
        (&ordinary, request()),
        (&ordinary, history.clone()),
        (&replay, history),
        (&ordinary, capped),
        (&ordinary, structured),
    ] {
        provider
            .generate(req, Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap();
    }
    let expected: &[&[u8]] = &[
        br#"{"messages":[{"content":"System.","role":"system"},{"content":"Hello.","role":"user"}],"model":"chat-model","reasoning_effort":"low","stream":true,"stream_options":{"include_usage":true}}"#,
        br#"{"messages":[{"content":"System.","role":"system"},{"content":"Hello.","role":"user"},{"content":"Prior.","role":"assistant"}],"model":"chat-model","reasoning_effort":"low","stream":true,"stream_options":{"include_usage":true}}"#,
        br#"{"messages":[{"content":"System.","role":"system"},{"content":"Hello.","role":"user"},{"content":"Prior.","reasoning_content":"Exact reasoning.","role":"assistant"}],"model":"chat-model","reasoning_effort":"low","stream":true,"stream_options":{"include_usage":true}}"#,
        br#"{"max_tokens":42,"messages":[{"content":"System.","role":"system"},{"content":"Hello.","role":"user"}],"model":"chat-model","reasoning_effort":"low","stream":true,"stream_options":{"include_usage":true}}"#,
        br#"{"messages":[{"content":"System.","role":"system"},{"content":"Hello.","role":"user"}],"model":"chat-model","reasoning_effort":"low","stream":true,"stream_options":{"include_usage":true}}"#,
    ];
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), expected.len());
    for ((headers, actual), expected) in requests.iter().zip(expected) {
        assert_eq!(actual.as_slice(), *expected);
        assert_eq!(headers["authorization"], "Bearer synthetic-key");
    }
    server.abort();
}

#[tokio::test]
async fn chat_session_header_is_sent_only_on_opencode_routes() {
    let (base, captured, server) = capture_server().await;
    let plain = ChatCompletionsProvider::api_key("synthetic-key", &base).unwrap();
    plain
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    let opencode = ChatCompletionsProvider::api_key("synthetic-key", &base)
        .unwrap()
        .with_opencode_route();
    opencode
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    server.abort();
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].0.contains_key("x-opencode-session"));
    assert!(requests[1].0.contains_key("x-opencode-session"));
}
