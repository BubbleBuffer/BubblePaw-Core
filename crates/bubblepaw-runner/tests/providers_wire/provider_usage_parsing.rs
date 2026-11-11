use anyhow::Result;
use axum::{Router, routing::post};
use bubblepaw_core::{
    CancellationToken, DecisionProvider, DecisionRequest, Message, ModelRequest, Provider,
    ProviderError, Question, QuestionKind, RequestKind, Role, Store, Usage, capture_request_usage,
};
use bubblepaw_runner::{
    model_roles::ModelRole,
    provider::{
        MessagesProvider, OpenAiDecisions, OpenCodeDecisions, OpenRouterDecisions,
        ProviderRegistry, ScriptedProvider,
    },
};
use serde_json::{Value, json};
use std::sync::Arc;

async fn serve(body: String, path: &'static str) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        path,
        post(move || {
            let body = body.clone();
            async move { ([("content-type", "application/json")], body) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, task)
}

async fn failed_messages_usage(end: Option<Value>) -> Result<()> {
    let mut events = vec![json!({
        "type":"message_start",
        "message":{"usage":{
            "input_tokens":19,"cache_read_input_tokens":11,
            "cache_creation_input_tokens":3,"output_tokens":0
        }}
    })];
    if let Some(end) = end {
        events.push(end);
    }
    let body: String = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
    // Messages appends /messages, so use a fixture route with that suffix.
    let app = Router::new().route(
        "/messages",
        post(move || {
            let body: String = body.clone();
            async move { ([("content-type", "text/event-stream")], body) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let provider =
        MessagesProvider::api_key("fixture-key", format!("http://{}", listener.local_addr()?))?;
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let store = Store::in_memory()?;
    let session = store.create_session("partial Messages usage")?.id;
    let (result, rows) = capture_request_usage(
        &session,
        None,
        RequestKind::Main,
        provider.generate(
            ModelRequest {
                model: "fixture".into(),
                session_id: session.clone(),
                messages: vec![Message {
                    id: "user".into(),
                    session_id: session.clone(),
                    role: Role::User,
                    origin: None,
                    content: "Hello".into(),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    provider_state: None,
                    is_error: false,
                    created_at: String::new(),
                }],
                ..Default::default()
            },
            Arc::new(|_| {}),
            CancellationToken::new(),
        ),
    )
    .await;
    task.abort();
    let _ = task.await;
    let error = result.unwrap_err();
    let expected = Usage {
        input_tokens: 33,
        cache_read_tokens: 11,
        cache_creation_tokens: 3,
        ..Default::default()
    };
    assert_eq!(
        ProviderError::of(&error).unwrap().usage.as_ref(),
        Some(&expected)
    );
    assert_eq!(rows.len(), 1);
    for row in rows {
        store.queue_request_usage(row)?;
    }
    store.flush_request_usage(&session)?;
    let rows = store.request_usage()?;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].failed);
    assert_eq!(rows[0].usage, expected);
    assert_eq!(rows[0].provider, "anthropic-messages");
    Ok(())
}

#[tokio::test]
async fn messages_error_event_preserves_input_and_cache_in_failed_row() -> Result<()> {
    failed_messages_usage(Some(json!({
        "type":"error","error":{"type":"api_error","message":"fixture failure"}
    })))
    .await
}

#[tokio::test]
async fn messages_premature_eof_preserves_input_and_cache_in_failed_row() -> Result<()> {
    failed_messages_usage(None).await
}

async fn decision_usage(kind: &str, deepseek: bool) -> Result<()> {
    let raw_usage = if deepseek {
        json!({"prompt_tokens":100,"completion_tokens":7,"prompt_cache_hit_tokens":61,
            "cache_creation_input_tokens":3,"completion_tokens_details":{"reasoning_tokens":2}})
    } else {
        json!({"input_tokens":100,"output_tokens":7,"input_tokens_details":{
            "cached_tokens":61,"cache_creation_tokens":3},"output_tokens_details":{"reasoning_tokens":2}})
    };
    let answers = if kind == "openai" {
        json!([{"name":"safe","type":"predicate","probability":1.0}])
    } else {
        json!({"safe":{"type":"choice","choice":"yes","probabilities":{"yes":1.0,"no":0.0}}})
    };
    let (base, task) = serve(
        json!({"model":"fixture","answers":answers,"usage":raw_usage}).to_string(),
        if kind == "openai" {
            "/v1/decisions"
        } else {
            "/fixture"
        },
    )
    .await;
    let endpoint = format!("{base}/fixture");
    let provider: Arc<dyn DecisionProvider> = match kind {
        "openai" => Arc::new(OpenAiDecisions::new("fixture-key", base)?),
        "openrouter" => Arc::new(OpenRouterDecisions::new("fixture-key", endpoint)?),
        "opencode" => Arc::new(OpenCodeDecisions::new("fixture-key", endpoint)?),
        _ => unreachable!(),
    };
    let store = Store::in_memory()?;
    let session = store.create_session("decision usage parsing")?.id;
    let mut registry = ProviderRegistry::new("unused", Arc::new(ScriptedProvider::new("unused")));
    registry.insert_decision(kind, provider)?;
    registry.set_usage_store(store.clone());
    let role = ModelRole {
        models: vec![format!("{kind}/fixture")],
        repeats: Some(2),
        threshold: None,
        reasoning_effort: Default::default(),
        max_output_tokens: None,
    };
    let result = role
        .decide(
            &registry,
            DecisionRequest {
                session_id: Some(session),
                questions: vec![Question {
                    name: "safe".into(),
                    instructions: "Safe?".into(),
                    kind: QuestionKind::Predicate,
                }],
                ..Default::default()
            },
            CancellationToken::new(),
        )
        .await;
    task.abort();
    let _ = task.await;
    let response = result?;
    assert_eq!(response.usage.input_tokens, 200);
    assert_eq!(response.usage.output_tokens, 14);
    assert_eq!(response.usage.cache_read_tokens, 122);
    assert_eq!(response.usage.cache_creation_tokens, 6);
    let rows = store.request_usage()?;
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert!(!row.failed);
        assert_eq!(row.provider, kind);
        assert_eq!(row.kind, RequestKind::Decider);
        assert_eq!(
            row.usage,
            Usage {
                input_tokens: 100,
                output_tokens: 7,
                cache_read_tokens: 61,
                cache_creation_tokens: 3,
                reasoning_tokens: Some(2),
            }
        );
    }
    Ok(())
}

#[tokio::test]
async fn openai_output_and_cache_reach_role_and_request_rows() -> Result<()> {
    decision_usage("openai", false).await
}

#[tokio::test]
async fn openrouter_output_and_cache_reach_role_and_request_rows() -> Result<()> {
    decision_usage("openrouter", false).await
}

#[tokio::test]
async fn opencode_output_and_cache_reach_role_and_request_rows() -> Result<()> {
    decision_usage("opencode", false).await
}

#[tokio::test]
async fn deepseek_cache_hits_reach_role_and_request_rows() -> Result<()> {
    decision_usage("opencode", true).await
}
