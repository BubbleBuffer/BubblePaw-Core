//! Unit tests of the adapter.
use super::*;
use std::sync::Arc;

#[tokio::test]
async fn stream_envelope_is_not_repeated_per_token_but_extensions_survive() -> Result<()> {
    let mut body = String::new();
    for index in 0..256 {
        let mut delta = json!({"content":"x", "reasoning_content":"thought "});
        if matches!(index, 4 | 5) {
            delta["vendor_signature_delta"] = json!("same opaque fragment");
        }
        let event = json!({"id":"completion-1", "object":"chat.completion.chunk",
                "created":123, "model":"test", "system_fingerprint":"fp",
                "choices":[{"index":0,"delta":delta,"finish_reason":null}]});
        body.push_str(&format!("data: {event}\n\n"));
    }
    body.push_str("data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":512}}\n\ndata: [DONE]\n\n");
    let app = axum::Router::new().route(
        "/chat/completions",
        axum::routing::post(move || {
            let body = body.clone();
            async move { ([("content-type", "text/event-stream")], body) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let provider = ChatCompletionsProvider::without_key(base)?;
    let result = provider
        .generate(
            ModelRequest {
                model: "test".into(),
                ..Default::default()
            },
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await;
    server.abort();
    let response = result?;
    assert_eq!(response.text, "x".repeat(256));
    let state = response.provider_state.unwrap();
    assert_eq!(response.thinking, "thought ".repeat(256));
    assert_eq!(
        state["message_fields"]["reasoning_content"],
        "thought ".repeat(256)
    );
    // The registry still reads Chat reasoning from the state.
    assert_eq!(
        crate::provider::ProviderRegistry::reasoning(&state),
        Some("thought ".repeat(256))
    );
    let fragments = state["opaque_stream_fragments"].as_array().unwrap();
    assert_eq!(
        fragments.len(),
        2,
        "ordinary SSE framing must not become a token-by-token archive"
    );
    for fragment in fragments {
        assert_eq!(
            fragment["choices"][0]["delta"]["vendor_signature_delta"],
            "same opaque fragment"
        );
        assert!(fragment.get("id").is_none());
    }
    assert_eq!(state["response_fields"]["id"], "completion-1");
    assert_eq!(state["response_fields"]["model"], "test");
    assert_eq!(state["response_fields"]["system_fingerprint"], "fp");
    Ok(())
}

#[test]
fn counting_large_canonical_journal_does_not_change_native_chat_wire() -> Result<()> {
    let provider = ChatCompletionsProvider::without_key("http://127.0.0.1:1")?
        .with_reasoning_replay(BTreeMap::from([("test".into(), ReasoningReplay::All)]))?;
    let state = json!({"adapter":"chat-completions","message_fields":{"reasoning_content":"native reasoning"},
            "opaque_stream_fragments":[{"event":"x".repeat(3 * 1024 * 1024)}]});
    let mut message = bubblepaw_core::Message {
        id: "a".into(),
        session_id: "s".into(),
        role: Role::Assistant,
        origin: None,
        content: bubblepaw_core::text_blocks("visible"),
        provider_state: Some(state),
        created_at: "now".into(),
        ..Default::default()
    };
    let mut request = ModelRequest {
        model: "test".into(),
        messages: vec![message.clone()],
        ..Default::default()
    };
    let canonical = request.clone();
    let before = provider.wire_messages(&request)?;
    assert!(serde_json::to_vec(&request)?.len() > 2 * 1024 * 1024);
    assert!(bubblepaw_core::prepared_request_bytes(&request)? < 2 * 1024 * 1024);
    assert_eq!(request.messages, canonical.messages);
    assert_eq!(provider.wire_messages(&request)?, before);
    message
        .provider_state
        .as_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("opaque_stream_fragments");
    request.messages[0] = message;
    assert_eq!(provider.wire_messages(&request)?, before);
    assert_eq!(before[1]["reasoning_content"], "native reasoning");
    Ok(())
}

#[test]
fn wire_shape_hashes_the_sent_body_and_appends_extend_its_prefix() -> Result<()> {
    let provider = ChatCompletionsProvider::without_key("http://127.0.0.1:1")?;
    let message = |id: &str, content: &str| bubblepaw_core::Message {
        id: id.into(),
        session_id: "s".into(),
        role: Role::User,
        origin: None,
        content: bubblepaw_core::text_blocks(content),
        created_at: "now".into(),
        ..Default::default()
    };
    let first_request = ModelRequest {
        model: "test".into(),
        messages: vec![message("1", "a")],
        ..Default::default()
    };
    let second_request = ModelRequest {
        model: "test".into(),
        messages: vec![message("1", "a"), message("2", "b")],
        ..Default::default()
    };
    let first = provider.wire_shape(&first_request).unwrap();
    let second = provider.wire_shape(&second_request).unwrap();
    // Appending one message keeps every earlier boundary and adds one.
    assert!(second.boundaries.starts_with(&first.boundaries));
    assert_eq!(second.boundaries.len(), first.boundaries.len() + 1);
    assert!(second.wire_bytes > first.wire_bytes);
    // The shape is built from the same body helper that `round` posts.
    let body = provider.round_body(&second_request)?;
    assert_eq!(body["model"], json!("test"));
    assert_eq!(body["stream"], json!(true));
    let mut expected = bubblepaw_core::WireShape::from_body(
        &body,
        "messages",
        bubblepaw_core::DEFAULT_CACHE_BLOCK_TOKENS,
    );
    expected.model = "test".into();
    assert_eq!(second, expected);
    assert!(
        second.provider.is_empty(),
        "adapters leave the route id empty"
    );
    Ok(())
}
