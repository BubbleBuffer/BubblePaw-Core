use super::*;

fn tool_result_is_error(fixture: &worker_jobs::Fixture) -> bool {
    fixture
        .store
        .messages(&fixture.session)
        .unwrap()
        .iter()
        .any(|message| {
            matches!(message.role, Role::Tool)
                && message.tool_call_id.as_deref() == Some("a")
                && message.is_error
        })
}

pub(super) async fn assert_call_recovery(call: Value, diagnostics: &[&str], deferred: bool) {
    let (base, requests, server) = serve(vec![
        reply(json!({"tool_calls":[call]}), "tool_calls"),
        reply(json!({"content":"Done."}), "stop"),
    ])
    .await;
    let provider = ChatCompletionsProvider::api_key("synthetic-key", base).unwrap();
    assert_recovery(provider, &requests, 1, diagnostics, deferred).await;
    server.abort();
}

async fn assert_recovery(
    provider: ChatCompletionsProvider,
    requests: &Captured,
    second_round: usize,
    diagnostics: &[&str],
    deferred: bool,
) {
    let tools = Arc::new(CountingTools::default());
    let mut registry = ToolRegistry::new();
    if deferred {
        registry.register_host(tools.clone()).unwrap();
        registry.mark_deferred("read");
    } else {
        registry.register(tools.clone()).unwrap();
    }
    let fixture = worker_jobs::Fixture::new(Arc::new(provider), registry, &tool_request().model);
    let run = fixture.run("Read a file.").await;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(run.text.as_deref(), Some("Done."));
    assert_eq!(tools.0.load(Ordering::SeqCst), 0);
    assert!(tool_result_is_error(&fixture));
    let request = body(requests, second_round);
    let sent = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "a")
        .expect("the model receives the rejected call's result");
    let content = sent["content"].as_str().unwrap();
    for text in diagnostics {
        assert!(content.contains(text), "{text} missing from {content}");
    }
    assert_eq!(requests.lock().unwrap().len(), second_round + 1);
}

#[tokio::test]
async fn malformed_stream_arguments_keep_usage_and_get_an_error_result() {
    let events = [
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"read","arguments":"{bad"}}]},"finish_reason":"tool_calls"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":4,"cache_creation_tokens":2},"completion_tokens_details":{"reasoning_tokens":1}}}),
    ];
    let (base, requests, server) = serve(vec![
        sse(&events),
        sse(&events),
        reply(json!({"content":"Done."}), "stop"),
    ])
    .await;
    let provider = ChatCompletionsProvider::api_key("key", base).unwrap();
    let result = provider
        .generate(
            tool_request(),
            Arc::new(|_| panic!("no text")),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let usage = result.usage.as_ref().unwrap();
    assert_eq!(usage.input_tokens, 11);
    assert_eq!(usage.output_tokens, 3);
    assert_eq!(usage.cache_read_tokens, 4);
    assert_eq!(usage.cache_creation_tokens, 2);
    assert_eq!(usage.reasoning_tokens, Some(1));
    assert_recovery(
        provider,
        &requests,
        2,
        &["not valid JSON", "line 1 column 2"],
        false,
    )
    .await;
    server.abort();
}
