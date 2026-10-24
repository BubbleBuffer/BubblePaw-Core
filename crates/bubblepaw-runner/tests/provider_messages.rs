#[path = "support/chat_messages.rs"]
mod chat_messages;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use bubblepaw_core::{
    AnswerKind, CancellationToken, DecisionChoice, DecisionProvider, DecisionRequest, ModelRequest,
    Provider, ProviderError, ProviderErrorKind, Question, QuestionKind, ReasoningEffort, Role,
    ScoreLevel, ToolDefinition, provider_error_retryable,
};
use bubblepaw_runner::provider::{
    LlmDecisions, MessagesProvider, ProviderRegistry, ScriptedProvider, decision_error_usage,
    provider_retry_after,
};
use chat_messages::fixture_message;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone)]
struct Fixture {
    replies: Arc<Mutex<VecDeque<(StatusCode, Value)>>>,
    requests: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
}
async fn serve(
    replies: Vec<(StatusCode, Value)>,
) -> (String, Fixture, tokio::task::JoinHandle<()>) {
    let fixture = Fixture {
        replies: Arc::new(Mutex::new(replies.into())),
        requests: Arc::default(),
    };
    let app = Router::new().route("/v1/messages",post(|State(state):State<Fixture>,headers:HeaderMap,Json(body):Json<Value>|async move {
        state.requests.lock().unwrap().push((headers,body));
        let (status,payload) = state.replies.lock().unwrap().pop_front().expect("unexpected retry");
        (status,[("retry-after","3")],Json(payload)).into_response()
    })).with_state(fixture.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, fixture, task)
}

async fn serve_events(events: Vec<Value>) -> (String, Fixture, tokio::task::JoinHandle<()>) {
    let fixture = Fixture {
        replies: Arc::default(),
        requests: Arc::default(),
    };
    let body: String = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
    let app = Router::new()
        .route(
            "/v1/messages",
            post(
                move |State(state): State<Fixture>,
                      headers: HeaderMap,
                      Json(request): Json<Value>| {
                    let body = body.clone();
                    async move {
                        state.requests.lock().unwrap().push((headers, request));
                        ([("content-type", "text/event-stream")], body)
                    }
                },
            ),
        )
        .with_state(fixture.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, fixture, task)
}

/// Writes the events as one event stream, then closes the socket with no
/// terminal event after them. The request is read in full so the close is clean.
async fn serve_cut_events(events: Vec<Value>) -> (String, tokio::task::JoinHandle<()>) {
    let body: String = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(socket.read_u8().await.unwrap());
        }
        let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut request = vec![0; length];
        socket.read_exact(&mut request).await.unwrap();
        let reply = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
        );
        socket.write_all(reply.as_bytes()).await.unwrap();
        // Dropping the socket here is the cut: nothing follows the last event.
    });
    (base, task)
}
fn request() -> ModelRequest {
    ModelRequest {
        model: "claude-haiku-5-5".into(),
        system_prompt: "Summarize the result.".into(),
        messages: vec![fixture_message(Role::User, "Read a and b.")],
        tools: vec![ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
            read_only: true,
            approval: Default::default(),
        }],
        ..Default::default()
    }
}
fn payload(content: Value, stop: &str) -> Value {
    json!({"type":"message","role":"assistant","content":content,"stop_reason":stop,"usage":{"input_tokens":10,"output_tokens":4,"cache_read_input_tokens":20,"cache_creation_input_tokens":30}})
}
#[tokio::test]
async fn parallel_tools_and_thinking_round_trip_preserve_native_content() {
    let content = json!([
        {"type":"thinking","thinking":"I should read both.","signature":"synthetic-sig","extra":{"untouched":true}},
        {"type":"redacted_thinking","data":"synthetic-redacted"},
        {"type":"text","text":"Reading."},
        {"type":"tool_use","id":"a","name":"read","input":{"path":"a"}},
        {"type":"tool_use","id":"b","name":"read","input":{"path":"b"}},
        {"type":"text","text":" And waiting."}
    ]);
    let (base, fixture, server) = serve(vec![
        (StatusCode::OK, payload(content.clone(), "tool_use")),
        (
            StatusCode::OK,
            payload(json!([{"type":"text","text":"Done."}]), "end_turn"),
        ),
    ])
    .await;
    let provider = MessagesProvider::api_key("synthetic-key", format!("{base}/"))
        .unwrap()
        .with_opencode_route();
    let mut req = request();
    req.session_id = "explicit-session".into();
    req.messages.insert(
        0,
        fixture_message(Role::System, "Additional system guidance."),
    );
    let output = Arc::new(Mutex::new(String::new()));
    let sink = output.clone();
    let first = provider
        .generate(
            req.clone(),
            Arc::new(move |text| sink.lock().unwrap().push_str(text)),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(first.text, "Reading. And waiting.");
    assert_eq!(*output.lock().unwrap(), "Reading. And waiting.");
    assert_eq!(first.tool_calls.len(), 2);
    assert_eq!(first.tool_calls[1].arguments, json!({"path":"b"}));
    let usage = first.usage.as_ref().unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens
        ),
        (60, 4, 20)
    );
    assert_eq!(usage.cache_creation_tokens, 30);
    assert_eq!(usage.reasoning_tokens, None);
    let compact = first.provider_state.as_ref().unwrap();
    assert!(compact["content"][2].get("text").is_none());
    assert!(compact["content"][3].get("input").is_none());
    assert_eq!(
        first.provider_state.as_ref().unwrap()["cache_creation_input_tokens"],
        30
    );
    let mut assistant = fixture_message(Role::Assistant, &first.text);
    assistant.tool_calls = first.tool_calls;
    assistant.provider_state = first.provider_state;
    req.messages.push(assistant);
    let mut a = fixture_message(Role::Tool, "file a");
    a.tool_call_id = Some("a".into());
    let mut b = fixture_message(Role::Tool, "file b missing");
    b.tool_call_id = Some("b".into());
    b.is_error = true;
    req.messages.extend([a, b]);
    req.max_output_tokens = Some(222);
    req.disabled_tools = std::mem::take(&mut req.tools);
    req.cache_tiers.get_or_insert_default().previous_len = Some(2);
    let second = provider
        .generate(req, Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(second.text, "Done.");
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let (headers, body) = &requests[0];
    assert_eq!(headers["x-api-key"], "synthetic-key");
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(headers["x-opencode-session"], "explicit-session");
    assert_eq!(
        requests[1].0["x-opencode-session"],
        headers["x-opencode-session"]
    );
    assert!(!headers.contains_key("authorization"));
    assert_eq!(body["model"], "claude-haiku-5-5");
    assert_eq!(body["thinking"], json!({"type":"adaptive"}));
    assert_eq!(body["output_config"], json!({"effort":"low"}));
    assert_eq!(body["max_tokens"], 128000);
    assert_eq!(body["stream"], true);
    assert_eq!(body["system"][0]["text"], "Summarize the result.");
    assert_eq!(body["system"].as_array().unwrap().len(), 1);
    let first = &body["messages"][0]["content"];
    assert_eq!(first[0]["text"], "Additional system guidance.");
    assert_eq!(
        body["tools"][0]["input_schema"],
        request().tools[0].input_schema
    );
    assert_eq!(body["tool_choice"], json!({"type":"auto"}));
    for name in ["temperature", "top_p", "top_k"] {
        assert!(body.get(name).is_none());
    }
    assert_eq!(first[1]["cache_control"], json!({"type":"ephemeral"}));
    let second_body = &requests[1].1;
    assert_eq!(second_body["tools"], body["tools"]);
    assert_eq!(second_body["tool_choice"], json!({"type":"none"}));
    assert_eq!(second_body["messages"][0], body["messages"][0]);
    assert_eq!(second_body["messages"][1]["content"], content);
    assert_eq!(
        second_body["messages"][2],
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"file a","is_error":false},{"type":"tool_result","tool_use_id":"b","content":"file b missing","is_error":true,"cache_control":{"type":"ephemeral"}}]})
    );
    assert_eq!(second_body["messages"].as_array().unwrap().len(), 3);
    assert_eq!(second_body["max_tokens"], 222);
    server.abort();
}

#[tokio::test]
async fn refusal_is_a_metered_fatal_error_without_retries() {
    let reason = "refusal";
    let (base, fixture, server) = serve(vec![(StatusCode::OK, payload(json!([]), reason))]).await;
    let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
    let output = Arc::new(Mutex::new(String::new()));
    let sink = output.clone();
    let error = provider
        .generate(
            request(),
            Arc::new(move |text| sink.lock().unwrap().push_str(text)),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains(reason));
    assert_eq!(
        ProviderError::of(&error).unwrap().kind,
        ProviderErrorKind::Fatal
    );
    let usage = decision_error_usage(&error).unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (60, 4));
    assert!(output.lock().unwrap().is_empty());
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn summaries_omit_tools_and_accept_output_length_as_guidance() {
    let text = "x".repeat(70_000);
    let (base,fixture,server) = serve(vec![(StatusCode::OK,json!({"content":[{"type":"text","text":text}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":3}}))]).await;
    let provider = MessagesProvider::api_key("synthetic-key", base)
        .unwrap()
        .with_opencode_route();
    let mut req = request();
    req.tools.clear();
    req.messages[0].session_id.clear();
    req.messages[0].id.clear();
    req.max_output_tokens = Some(1);
    let response = provider
        .generate(req, Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.text, text);
    let usage = response.usage.unwrap();
    assert_eq!(
        (usage.cache_read_tokens, usage.cache_creation_tokens),
        (0, 0)
    );
    let requests = fixture.requests.lock().unwrap();
    let body = &requests[0].1;
    assert!(uuid::Uuid::parse_str(requests[0].0["x-opencode-session"].to_str().unwrap()).is_ok());
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
    assert_eq!(body["max_tokens"], 1);
    server.abort();
}

#[tokio::test]
async fn rebuilt_history_can_start_with_assistant_and_preserves_raw_blocks() {
    let (base, fixture, server) = serve(vec![(
        StatusCode::OK,
        payload(json!([{"type":"text","text":"Continued."}]), "end_turn"),
    )])
    .await;
    let provider = MessagesProvider::api_key("synthetic-key", base)
        .unwrap()
        .with_opencode_route();
    let mut req = request();
    let content = json!([{"type":"thinking","thinking":"Earlier reasoning.","signature":"synthetic-signature"},{"type":"text","text":"Earlier summary."}]);
    let mut assistant = fixture_message(Role::Assistant, "Earlier summary.");
    assistant.provider_state = Some(json!({"adapter":"anthropic-messages","content":content}));
    req.messages.insert(0, assistant);
    provider
        .generate(req, Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    let requests = fixture.requests.lock().unwrap();
    let body = &requests[0].1;
    assert_eq!(body["messages"][0]["role"], "assistant");
    assert_eq!(body["messages"][0]["content"], content);
    assert_eq!(
        body["messages"][1]["content"][0]["cache_control"],
        json!({"type":"ephemeral"})
    );
    assert_eq!(requests[0].0["x-opencode-session"], "fixture-session");
    server.abort();
}

#[tokio::test]
async fn messages_effort_override_keeps_adaptive_thinking_on() {
    let efforts = [
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::XHigh,
        ReasoningEffort::Max,
    ];
    let replies = efforts
        .iter()
        .map(|_| {
            (
                StatusCode::OK,
                payload(json!([{"type":"text","text":"Result."}]), "end_turn"),
            )
        })
        .collect();
    let (base, fixture, server) = serve(replies).await;
    let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
    for effort in efforts {
        let mut req = request();
        req.reasoning_effort = Some(effort);
        provider
            .generate(req, Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap();
    }
    for ((_, body), effort) in fixture.requests.lock().unwrap().iter().zip(efforts) {
        assert_eq!(body["output_config"]["effort"], effort.as_str());
        assert_eq!(body["thinking"], json!({"type":"adaptive"}));
        for name in ["temperature", "top_p", "top_k"] {
            assert!(body.get(name).is_none());
        }
    }
    server.abort();
}

fn decision_request() -> DecisionRequest {
    DecisionRequest {
        model: "llm/opencode-messages/claude-haiku-5-5".into(),
        input: "Synthetic decision fixture.".into(),
        session_id: Some("decision-session".into()),
        reasoning_effort: ReasoningEffort::Low,
        questions: vec![
            Question {
                name: "yes".into(),
                instructions: "True?".into(),
                kind: QuestionKind::Predicate,
            },
            Question {
                name: "no".into(),
                instructions: "False?".into(),
                kind: QuestionKind::Predicate,
            },
            Question {
                name: "choice".into(),
                instructions: "Which?".into(),
                kind: QuestionKind::Choice {
                    choices: vec![
                        DecisionChoice {
                            value: "one".into(),
                            description: "One".into(),
                        },
                        DecisionChoice {
                            value: "two".into(),
                            description: "Two".into(),
                        },
                    ],
                },
            },
            Question {
                name: "score".into(),
                instructions: "Severity?".into(),
                kind: QuestionKind::Score {
                    levels: vec![
                        ScoreLevel {
                            label: "low".into(),
                            description: "Minor".into(),
                        },
                        ScoreLevel {
                            label: "high".into(),
                            description: "Major".into(),
                        },
                    ],
                },
            },
        ],
    }
}
#[tokio::test]
async fn registry_llm_messages_decisions_are_hard_samples_with_usage_and_refusal() {
    let text = r#"{"yes":true,"no":false,"choice":"one","score":"high"}"#;
    let (base, fixture, server) = serve(vec![
        (
            StatusCode::OK,
            payload(json!([{"type":"text","text":text}]), "end_turn"),
        ),
        (
            StatusCode::OK,
            payload(json!([{"type":"text","text":text}]), "end_turn"),
        ),
        (
            StatusCode::OK,
            payload(json!([{"type":"text","text":"not JSON"}]), "end_turn"),
        ),
    ])
    .await;
    let mut registry = ProviderRegistry::new("main", Arc::new(ScriptedProvider::new("unused")));
    registry
        .insert(
            "opencode-messages",
            Arc::new(
                MessagesProvider::api_key("synthetic-key", base)
                    .unwrap()
                    .with_opencode_route(),
            ),
        )
        .unwrap();
    for effort in [ReasoningEffort::Low, ReasoningEffort::High] {
        let mut req = decision_request();
        req.reasoning_effort = effort;
        let response = DecisionProvider::decide(&registry, req, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(response.model, "claude-haiku-5-5");
        assert_eq!(
            (response.usage.input_tokens, response.usage.output_tokens),
            (60, 4)
        );
        assert!(matches!(
            response.answers[0].kind,
            AnswerKind::Predicate { probability: 1.0 }
        ));
        assert!(matches!(
            response.answers[1].kind,
            AnswerKind::Predicate { probability: 0.0 }
        ));
        assert!(
            matches!(&response.answers[2].kind,AnswerKind::Choice {choice,probabilities} if choice=="one" && probabilities==&vec![("one".into(),1.0),("two".into(),0.0)])
        );
        assert!(
            matches!(&response.answers[3].kind,AnswerKind::Score {score:1.0,probabilities} if probabilities==&vec![("low".into(),0.0),("high".into(),1.0)])
        );
    }
    let refusal = DecisionProvider::decide(&registry, decision_request(), CancellationToken::new())
        .await
        .unwrap();
    assert!(
        refusal
            .answers
            .iter()
            .all(|answer| matches!(answer.kind, AnswerKind::Refusal))
    );
    assert_eq!(
        (refusal.usage.input_tokens, refusal.usage.output_tokens),
        (60, 4)
    );
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for ((headers, body), effort) in requests.iter().zip(["low", "high", "low"]) {
        assert_eq!(body["model"], "claude-haiku-5-5");
        assert_eq!(body["output_config"]["effort"], effort);
        assert_eq!(body["thinking"], json!({"type":"adaptive"}));
        assert_eq!(headers["x-opencode-session"], "decision-session");
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        assert_eq!(
            body["system"][0]["text"],
            LlmDecisions::prompt(&decision_request()).unwrap()
        );
    }
    server.abort();
}

#[tokio::test]
async fn configured_summary_alias_drives_high_effort_messages_decision_votes() {
    use bubblepaw_runner::config::{EffectiveConfig, LoadOptions, Override};
    let root = tempfile::tempdir().unwrap();
    let settings = EffectiveConfig::load(LoadOptions {
        workspace: root.path().to_owned(),
        home: None,
        custom_file: None,
        env: Vec::new(),
        cli: vec![
            Override::set(r#"roles.summary=["opencode-messages/claude-haiku-5-5"]"#).unwrap(),
            Override::set(
                r#"roles.decider={models=["llm/summary"],repeats=3,reasoning_effort="high"}"#,
            )
            .unwrap(),
        ],
    })
    .unwrap()
    .settings()
    .unwrap();
    assert_eq!(
        settings.roles.decider.models,
        ["llm/opencode-messages/claude-haiku-5-5"]
    );
    let replies = [true, false, true]
        .iter()
        .map(|value| {
            (
                StatusCode::OK,
                payload(
                    json!([{"type":"text","text":json!({"yes":value}).to_string()}]),
                    "end_turn",
                ),
            )
        })
        .collect();
    let (base, fixture, server) = serve(replies).await;
    let mut registry = ProviderRegistry::new("main", Arc::new(ScriptedProvider::new("unused")));
    registry
        .insert(
            "opencode-messages",
            Arc::new(MessagesProvider::api_key("synthetic-key", base).unwrap()),
        )
        .unwrap();
    let mut req = decision_request();
    req.model.clear();
    req.questions.truncate(1);
    let response = settings
        .roles
        .decider
        .decide(&registry, req, CancellationToken::new())
        .await
        .unwrap();
    assert!(
        matches!(response.answers[0].kind,AnswerKind::Predicate {probability} if (probability-2.0/3.0).abs()<1e-12)
    );
    assert_eq!(
        (response.usage.input_tokens, response.usage.output_tokens),
        (180, 12)
    );
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for (_, body) in requests.iter() {
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["thinking"], json!({"type":"adaptive"}));
        assert!(body.get("tools").is_none());
    }
    server.abort();
}

#[tokio::test]
async fn fake_400_includes_bounded_validation_body_and_redacts_auth() {
    let payload = json!({"type":"error","error":{
        "type":"invalid_request_error",
        "message":"thinking block required; echoed synthetic-key and Bearer synthetic-auth",
        "details":{"field":"messages.1.content.0", "api_key":"nested-secret"},
        "padding":"é".repeat(5000)
    }});
    let (base, fixture, server) = serve(vec![(StatusCode::BAD_REQUEST, payload)]).await;
    let error = MessagesProvider::api_key("synthetic-key", base)
        .unwrap()
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        ProviderError::of(&error).unwrap().kind,
        ProviderErrorKind::Fatal
    );
    let diagnostic = error.to_string();
    assert!(diagnostic.starts_with("Messages HTTP 400: type=invalid_request_error"));
    assert!(diagnostic.contains("thinking block required"));
    assert!(diagnostic.contains("body="));
    assert!(diagnostic.contains("messages.1.content.0"));
    for secret in ["synthetic-key", "synthetic-auth", "nested-secret"] {
        assert!(!diagnostic.contains(secret), "{diagnostic}");
    }
    assert!(diagnostic.len() <= 4096);
    assert!(diagnostic.ends_with("[truncated]"));
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn http_errors_keep_typed_diagnostics_without_exposing_keys() {
    for (status, payload, expected) in [
        (
            StatusCode::UNAUTHORIZED,
            json!({"error":{"message":"synthetic rejection","api_key":"synthetic-key"}}),
            ProviderErrorKind::Auth,
        ),
        (
            StatusCode::FORBIDDEN,
            json!({"error":{"message":"synthetic rejection","api_key":"synthetic-key"}}),
            ProviderErrorKind::Auth,
        ),
        (
            StatusCode::BAD_REQUEST,
            json!({"error":{"message":"prompt is too long"}}),
            ProviderErrorKind::Context,
        ),
        (
            StatusCode::BAD_REQUEST,
            json!({"error":{"message":"synthetic rejection","api_key":"synthetic-key"}}),
            ProviderErrorKind::Fatal,
        ),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":{"message":"synthetic rejection","api_key":"synthetic-key"}}),
            ProviderErrorKind::Transient,
        ),
    ] {
        let expected_message = payload["error"]["message"].as_str().unwrap().to_owned();
        let (base, fixture, server) = serve(vec![(status, payload)]).await;
        let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
        let error = provider
            .generate(request(), Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(ProviderError::of(&error).unwrap().kind, expected);
        assert!(error.to_string().contains(&expected_message));
        assert!(!error.to_string().contains("synthetic-key"));
        assert_eq!(fixture.requests.lock().unwrap().len(), 1);
        server.abort();
    }
    let (base, fixture, server) = serve(vec![(StatusCode::TOO_MANY_REQUESTS, json!({}))]).await;
    let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
    let before = chrono::Utc::now();
    let error = provider
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap_err();
    let ProviderErrorKind::RateLimit {
        resets_at: Some(reset),
    } = &ProviderError::of(&error).unwrap().kind
    else {
        panic!("429 must retain its reset time");
    };
    let reset = chrono::DateTime::parse_from_rfc3339(reset).unwrap();
    assert!(reset >= before + chrono::Duration::seconds(3));
    assert!(reset <= chrono::Utc::now() + chrono::Duration::seconds(3));
    let delay = provider_retry_after(&error).expect("429 must retain Retry-After");
    assert!(delay > Duration::ZERO && delay <= Duration::from_secs(3));
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn streamed_usage_is_cumulative_and_reasoning_tokens_are_only_reported() {
    for reported in [None, Some(0), Some(7)] {
        let mut start = json!({"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":20,"cache_creation_input_tokens":30});
        let mut middle = json!({"output_tokens":5});
        let mut final_usage = json!({"output_tokens":9});
        if let Some(tokens) = reported {
            start["reasoning_tokens"] = json!(0);
            middle["reasoning_tokens"] = json!(tokens.min(3));
            final_usage["reasoning_tokens"] = json!(tokens);
        }
        let (base, fixture, server) = serve_events(vec![
            json!({"type":"message_start","message":{"usage":start}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"Synthetic reasoning has no estimated token count.","signature":"synthetic-signature"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Done."}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{},"usage":middle}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":final_usage}),
            json!({"type":"message_stop"}),
        ]).await;
        let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
        let response = provider
            .generate(request(), Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(response.text, "Done.");
        let usage = response.usage.unwrap();
        assert_eq!((usage.input_tokens, usage.output_tokens), (60, 9));
        assert_eq!(
            (usage.cache_read_tokens, usage.cache_creation_tokens),
            (20, 30)
        );
        assert_eq!(usage.reasoning_tokens, reported);
        assert_eq!(fixture.requests.lock().unwrap().len(), 1);
        server.abort();
    }
}

#[tokio::test]
async fn length_continuation_sums_only_reported_reasoning_usage() {
    for (first_tokens, second_tokens, expected) in [
        (None, None, None),
        (Some(2), Some(3), Some(5)),
        (Some(2), None, Some(2)),
        (None, Some(3), Some(3)),
    ] {
        let mut first = payload(json!([{"type":"text","text":"Part "}]), "max_tokens");
        let mut second = payload(json!([{"type":"text","text":"done."}]), "end_turn");
        if let Some(tokens) = first_tokens {
            first["usage"]["reasoning_tokens"] = json!(tokens);
        }
        if let Some(tokens) = second_tokens {
            second["usage"]["reasoning_tokens"] = json!(tokens);
        }
        let (base, fixture, server) =
            serve(vec![(StatusCode::OK, first), (StatusCode::OK, second)]).await;
        let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
        let response = provider
            .generate(request(), Arc::new(|_| {}), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(response.text, "Part done.");
        let usage = response.usage.unwrap();
        assert_eq!((usage.input_tokens, usage.output_tokens), (120, 8));
        assert_eq!(
            (usage.cache_read_tokens, usage.cache_creation_tokens),
            (40, 60)
        );
        assert_eq!(usage.reasoning_tokens, expected);
        assert_eq!(fixture.requests.lock().unwrap().len(), 2);
        server.abort();
    }
}

#[tokio::test]
async fn stream_errors_are_typed_without_exposing_server_text_or_retrying() {
    for (error_type, message, expected) in [
        (
            "authentication_error",
            "private-body",
            ProviderErrorKind::Auth,
        ),
        ("permission_error", "private-body", ProviderErrorKind::Auth),
        (
            "rate_limit_error",
            "private-body",
            ProviderErrorKind::RateLimit { resets_at: None },
        ),
        ("api_error", "private-body", ProviderErrorKind::Transient),
        (
            "timeout_error",
            "private-body",
            ProviderErrorKind::Transient,
        ),
        (
            "overloaded_error",
            "private-body",
            ProviderErrorKind::Transient,
        ),
        (
            "conflict_error",
            "private-body",
            ProviderErrorKind::Transient,
        ),
        (
            "invalid_request_error",
            "prompt is too long",
            ProviderErrorKind::Context,
        ),
        (
            "invalid_request_error",
            "private-body",
            ProviderErrorKind::Fatal,
        ),
        ("unknown_error", "private-body", ProviderErrorKind::Fatal),
    ] {
        for published in [false, true] {
            let mut events =
                vec![json!({"type":"message_start","message":{"usage":{"input_tokens":1}}})];
            if published {
                events.extend([
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Partial"}}),
                    json!({"type":"content_block_stop","index":0}),
                ]);
            }
            events.push(json!({"type":"error","error":{"type":error_type,"message":message}}));
            let (base, fixture, server) = serve_events(events).await;
            let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
            let output = Arc::new(Mutex::new(String::new()));
            let sink = output.clone();
            let error = provider
                .generate(
                    request(),
                    Arc::new(move |text| sink.lock().unwrap().push_str(text)),
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert_eq!(ProviderError::of(&error).unwrap().kind, expected);
            let typed = ProviderError::of(&error).unwrap();
            assert_eq!(typed.published, published);
            let retryable = !published
                && matches!(
                    expected,
                    ProviderErrorKind::Transient | ProviderErrorKind::RateLimit { .. }
                );
            assert_eq!(typed.retryable(), retryable);
            assert_eq!(provider_error_retryable(&error), retryable);
            assert!(
                error
                    .to_string()
                    .contains("Messages stream reported an error")
            );
            assert!(!error.to_string().contains(message));
            assert!(!error.to_string().contains("synthetic-key"));
            assert_eq!(
                *output.lock().unwrap(),
                if published { "Partial" } else { "" }
            );
            assert_eq!(fixture.requests.lock().unwrap().len(), 1);
            assert!(provider_retry_after(&error).is_none());
            server.abort();
        }
    }
}

#[tokio::test]
async fn partial_stream_usage_survives_error_event_and_cut() {
    // message_start reports input, cache read and cache creation; message_delta
    // reports later output. A failed stream keeps every count it already reported.
    let started = json!({"type":"message_start","message":{"usage":{"input_tokens":12,"output_tokens":1,"cache_read_input_tokens":40,"cache_creation_input_tokens":25}}});
    let text = vec![
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Partial"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{},"usage":{"output_tokens":7}}),
    ];
    let error_event =
        json!({"type":"error","error":{"type":"overloaded_error","message":"private-body"}});
    // (events, kind, published, output tokens reported before the failure)
    let cases = [
        (
            vec![started.clone(), error_event.clone()],
            ProviderErrorKind::Transient,
            false,
            1,
        ),
        (
            [vec![started.clone()], text.clone(), vec![error_event]].concat(),
            ProviderErrorKind::Transient,
            true,
            7,
        ),
        (
            [vec![started.clone()], text.clone()].concat(),
            ProviderErrorKind::Fatal,
            true,
            7,
        ),
        (
            vec![started.clone()],
            ProviderErrorKind::Transient,
            false,
            1,
        ),
    ];
    for (events, expected, published, output_tokens) in cases {
        let (base, server) = serve_cut_events(events).await;
        let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
        let received = Arc::new(Mutex::new(String::new()));
        let sink = received.clone();
        let error = provider
            .generate(
                request(),
                Arc::new(move |delta| sink.lock().unwrap().push_str(delta)),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let typed = ProviderError::of(&error).unwrap();
        assert_eq!(typed.kind, expected);
        assert_eq!(typed.published, published);
        assert_eq!(
            typed.retryable(),
            !published && expected == ProviderErrorKind::Transient
        );
        let usage = typed
            .usage
            .as_ref()
            .expect("failed stream keeps reported usage");
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens,
                usage.cache_creation_tokens,
            ),
            (77, output_tokens, 40, 25)
        );
        assert_eq!(
            *received.lock().unwrap(),
            if published { "Partial" } else { "" }
        );
        assert!(!error.to_string().contains("private-body"));
        server.abort();
    }
}

#[tokio::test]
async fn continuation_failure_keeps_publication_usage_and_retry_metadata() {
    for status in [
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::OK,
    ] {
        let second = if status == StatusCode::OK {
            payload(
                json!([{"type":"tool_use","id":"bad","name":"read","input":"bad"}]),
                "tool_use",
            )
        } else {
            json!({"error":{
                "message":"continuation temporarily unavailable",
                "api_key":"SERVER_SECRET_SENTINEL",
                "key_echo":"synthetic"
            }})
        };
        let (base, fixture, server) = serve(vec![
            (
                StatusCode::OK,
                payload(json!([{"type":"text","text":"Part "}]), "max_tokens"),
            ),
            (status, second),
        ])
        .await;
        let provider = MessagesProvider::api_key("synthetic", base).unwrap();
        let text = Arc::new(Mutex::new(String::new()));
        let sink = text.clone();
        let error = provider
            .generate(
                request(),
                Arc::new(move |delta| sink.lock().unwrap().push_str(delta)),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let typed = ProviderError::of(&error).unwrap();
        assert!(typed.published);
        assert!(!typed.retryable());
        assert!(!provider_error_retryable(&error));
        assert_eq!(*text.lock().unwrap(), "Part ");
        assert_eq!(fixture.requests.lock().unwrap().len(), 2);
        assert!(!format!("{error:?}").contains("SERVER_SECRET_SENTINEL"));
        if status != StatusCode::OK {
            let diagnostic = error.to_string();
            assert!(diagnostic.contains("continuation temporarily unavailable"));
            assert!(diagnostic.contains("body="));
            assert!(!diagnostic.contains("synthetic"));
        }
        match status {
            StatusCode::INTERNAL_SERVER_ERROR => {
                assert_eq!(typed.kind, ProviderErrorKind::Transient)
            }
            StatusCode::TOO_MANY_REQUESTS => {
                assert!(matches!(
                    typed.kind,
                    ProviderErrorKind::RateLimit { resets_at: Some(_) }
                ));
                assert!(provider_retry_after(&error).is_some());
            }
            _ => {
                assert_eq!(typed.kind, ProviderErrorKind::Fatal);
                let usage = decision_error_usage(&error)
                    .expect("publication context retains metered usage");
                assert_eq!((usage.input_tokens, usage.output_tokens), (60, 4));
            }
        }
        server.abort();
    }
}

#[tokio::test]
async fn malformed_tool_calls_are_metered_and_do_not_publish_text() {
    for content in [
        json!([{"type":"tool_use","id":"a","name":"read","input":"bad"}]),
        json!([{"type":"tool_use","id":"a","name":"read","input":{}},{"type":"tool_use","id":"a","name":"read","input":{}}]),
        json!([{"type":"tool_use","id":"a","name":"not-offered","input":{}}]),
    ] {
        let (base, _, server) = serve(vec![(StatusCode::OK, payload(content, "tool_use"))]).await;
        let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
        let error = provider
            .generate(
                request(),
                Arc::new(|_| panic!("invalid response published text")),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            ProviderError::of(&error).unwrap().kind,
            ProviderErrorKind::Fatal
        );
        assert_eq!(decision_error_usage(&error).unwrap().input_tokens, 60);
        server.abort();
    }
}

#[tokio::test]
async fn invalid_history_and_precancelled_calls_never_reach_transport() {
    let (base, fixture, server) = serve(Vec::new()).await;
    let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
    let mut req = request();
    req.messages
        .push(fixture_message(Role::Assistant, "unfinished"));
    let error = provider
        .generate(req, Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("prefill"));
    let mut req = request();
    req.messages.push(fixture_message(Role::Tool, "missing id"));
    let error = provider
        .generate(req, Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("call_id"));
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = provider
        .generate(request(), Arc::new(|_| {}), cancellation)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "cancelled");
    assert_eq!(
        ProviderError::of(&error).unwrap().kind,
        ProviderErrorKind::Transient
    );
    assert!(fixture.requests.lock().unwrap().is_empty());
    server.abort();
    assert!(MessagesProvider::api_key("", "http://localhost/v1").is_err());
    assert!(MessagesProvider::api_key("synthetic", "https://user:secret@localhost/v1").is_err());
}

#[tokio::test]
async fn in_flight_request_is_cancelable() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let entered2 = entered.clone();
    let app = Router::new().route(
        "/messages",
        post(move || {
            let entered = entered2.clone();
            async move {
                entered.notify_one();
                tokio::time::sleep(Duration::from_secs(20)).await;
                Json(payload(json!([{"type":"text","text":"late"}]), "end_turn"))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
    let cancellation = CancellationToken::new();
    let token = cancellation.clone();
    let pending = tokio::spawn(async move {
        provider
            .generate(
                request(),
                Arc::new(|_| panic!("cancelled response published text")),
                token,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    cancellation.cancel();
    let error = tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.to_string(), "cancelled");
    server.abort();
}

// Explicit opt-in acceptance probes. Never run by ordinary workspace gates.
fn live_provider() -> Option<MessagesProvider> {
    let Ok(key) = std::env::var("OPENCODE_API_KEY") else {
        println!("skipped: OPENCODE_API_KEY not set");
        return None;
    };
    Some(MessagesProvider::api_key(key, "https://opencode.ai/zen/go/v1").unwrap())
}
#[tokio::test]
#[ignore = "Makes one paid live model request; opt in explicitly"]
async fn live_summary_smoke() {
    let Some(provider) = live_provider() else {
        return;
    };
    let response = provider
        .generate(
            ModelRequest {
                model: "claude-haiku-5-5".into(),
                session_id: uuid::Uuid::new_v4().to_string(),
                system_prompt: "Summarize the user's note in one short sentence.".into(),
                messages: vec![fixture_message(
                    Role::User,
                    "The Messages adapter supports native tools and preserves thinking blocks.",
                )],
                ..Default::default()
            },
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!response.text.trim().is_empty());
    assert!(response.tool_calls.is_empty());
    println!("Summary smoke passed: {}", response.text);
}
#[path = "provider_messages/submission.rs"]
mod submission;

#[tokio::test]
#[ignore = "Makes two paid live model requests with one local tool round trip; opt in explicitly"]
async fn live_tool_smoke() {
    let Some(provider) = live_provider() else {
        return;
    };
    let mut req = ModelRequest {
        model:"claude-haiku-5-5".into(),
        session_id:uuid::Uuid::new_v4().to_string(),
        system_prompt:"Call the echo tool exactly once with text messages-smoke-ok. Once you receive its result, reply with that result and make no further calls.".into(),
        messages:vec![fixture_message(Role::User,"Run the echo check now.")],
        tools:vec![ToolDefinition {
            name:"echo".into(),description:"Return the supplied text unchanged.".into(),
            input_schema:json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}),
            read_only:true,approval:Default::default(),
        }],
        ..Default::default()
    };
    let first = provider
        .generate(req.clone(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(first.tool_calls.len(), 1, "Expected exactly one echo call");
    let call = &first.tool_calls[0];
    assert_eq!(call.name, "echo");
    let echoed = call.arguments["text"].as_str().expect("echo needs text");
    assert_eq!(echoed, "messages-smoke-ok");
    let mut result = fixture_message(Role::Tool, echoed);
    result.tool_call_id = Some(call.id.clone());
    let mut assistant = fixture_message(Role::Assistant, &first.text);
    assistant.tool_calls = first.tool_calls;
    assistant.provider_state = first.provider_state;
    req.messages.extend([assistant, result]);
    let second = provider
        .generate(req, Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    assert!(second.tool_calls.is_empty());
    assert!(second.text.contains("messages-smoke-ok"));
    println!("Tool smoke passed: {}", second.text);
}

#[tokio::test]
async fn messages_to_a_plain_anthropic_route_omits_the_session_header() {
    let (base, fixture, server) = serve(vec![(
        StatusCode::OK,
        payload(json!([{"type":"text","text":"OK"}]), "end_turn"),
    )])
    .await;
    let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
    provider
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].0.contains_key("x-opencode-session"));
    server.abort();
}
