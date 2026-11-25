use super::*;
use crate::worker_jobs;
use bubblepaw_core::{Tool, ToolContext, ToolRegistry, ToolResult};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct CountingTools(AtomicUsize);
#[async_trait::async_trait]
impl Tool for CountingTools {
    fn definition(&self) -> ToolDefinition {
        request().tools.remove(0)
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> anyhow::Result<ToolResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text("executed"))
    }
}

pub(super) async fn assert_call_recovery() {
    for (input, name, diagnostics) in [
        (
            json!("bad"),
            "read",
            &["not valid JSON", "line 1 column 1"][..],
        ),
        (json!([]), "read", &["expected a JSON object"][..]),
        (
            json!({}),
            "raed",
            &["Unknown tool", "Closest available tools: read"][..],
        ),
    ] {
        let content = json!([{"type":"tool_use","id":"a","name":name,"input":input}]);
        let (base, mock, server) = serve(vec![
            (StatusCode::OK, payload(content, "tool_use")),
            (
                StatusCode::OK,
                payload(json!([{"type":"text","text":"Done."}]), "end_turn"),
            ),
        ])
        .await;
        let tools = Arc::new(CountingTools::default());
        let mut registry = ToolRegistry::new();
        registry.register(tools.clone()).unwrap();
        let provider = MessagesProvider::api_key("synthetic-key", base).unwrap();
        let fixture = worker_jobs::Fixture::new(Arc::new(provider), registry, &request().model);
        let run = fixture.run("Read a file.").await;
        assert_eq!(run.status, "completed", "{:?}", run.error);
        assert_eq!(run.text.as_deref(), Some("Done."));
        assert_eq!(tools.0.load(Ordering::SeqCst), 0);
        let stored = fixture.store.messages(&fixture.session).unwrap();
        assert!(stored.iter().any(|message| {
            matches!(message.role, Role::Tool)
                && message.tool_call_id.as_deref() == Some("a")
                && message.is_error
        }));
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let blocks: Vec<_> = requests[1].1["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|message| message["content"].as_array())
            .flatten()
            .collect();
        let call = blocks
            .iter()
            .find(|block| block["type"] == "tool_use" && block["id"] == "a")
            .unwrap();
        assert_eq!(call["input"], json!({}));
        assert!(
            stored
                .iter()
                .flat_map(|message| &message.tool_calls)
                .any(|call| { call.id == "a" && call.arguments == input })
        );
        let result = blocks
            .iter()
            .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == "a")
            .unwrap();
        assert_eq!(result["is_error"], true);
        let sent = result["content"].to_string();
        for text in diagnostics {
            assert!(sent.contains(text), "{text} missing from {sent}");
        }
        server.abort();
    }

    let (base, _, server) = serve_events(vec![
        json!({"type":"message_start","message":{"usage":{"input_tokens":10}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"array","name":"read","input":[]}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"raw","name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{bad"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ])
    .await;
    let reply = MessagesProvider::api_key("synthetic-key", base)
        .unwrap()
        .generate(request(), Arc::new(|_| {}), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(reply.tool_calls[0].arguments, json!([]));
    assert_eq!(reply.tool_calls[1].arguments, json!("{bad"));
    server.abort();
}
