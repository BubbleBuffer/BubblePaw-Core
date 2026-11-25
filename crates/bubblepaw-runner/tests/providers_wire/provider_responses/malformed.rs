use super::*;
use crate::worker_jobs;
use bubblepaw_core::{Tool, ToolContext, ToolRegistry, ToolResult};

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

fn stream(output: &Value) -> String {
    let mut events: Vec<Value> = output
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(index, item)| json!({"type":"response.output_item.done","output_index":index,"item":item}))
        .collect();
    events.push(
        json!({"type":"response.completed","response":{"status":"completed","output":output}}),
    );
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

pub(super) async fn assert_call_recovery() {
    for (malformed, diagnostics) in [
        (
            json!({"type":"function_call","call_id":"call-2","name":"workspace_read","arguments":"{bad-json"}),
            &["not valid JSON (key must be a string at line 1 column 2)"][..],
        ),
        (
            json!({"type":"function_call","call_id":"call-2","name":"workspace_read","arguments":"true"}),
            &["expected a JSON object"][..],
        ),
        (
            json!({"type":"function_call","call_id":"call-2","name":"workspace_read","arguments":"[]"}),
            &["expected a JSON object"][..],
        ),
        (
            json!({"type":"function_call","call_id":"call-2","name":"workspace_read","arguments":"null"}),
            &["expected a JSON object"][..],
        ),
        (
            json!({"type":"function_call","call_id":"call-2","name":"workspace_reed","arguments":"{}"}),
            &["Unknown tool", "Closest available tools: workspace_read"][..],
        ),
    ] {
        let replies = Arc::new(Mutex::new(vec![
            stream(
                &json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done."}]}]),
            ),
            stream(&json!([malformed])),
        ]));
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let app = Router::new().route(
            "/responses",
            post({
                let (replies, requests) = (replies.clone(), requests.clone());
                move |Json(body): Json<Value>| {
                    let (replies, requests) = (replies.clone(), requests.clone());
                    async move {
                        requests.lock().unwrap().push(body);
                        let reply = replies.lock().unwrap().pop().expect("unexpected retry");
                        ([("content-type", "text/event-stream")], reply)
                    }
                }
            }),
        );
        let (base, server) = serve(app).await;
        let provider = ResponsesProvider::api_key("synthetic-key", base).unwrap();
        let tools = Arc::new(CountingTools::default());
        let mut registry = ToolRegistry::new();
        registry.register(tools.clone()).unwrap();
        let fixture = worker_jobs::Fixture::new(Arc::new(provider), registry, &request().model);
        let run = fixture.run("Read a file.").await;
        assert_eq!(run.status, "completed", "{:?}", run.error);
        assert_eq!(run.text.as_deref(), Some("Done."));
        assert_eq!(tools.0.load(Ordering::SeqCst), 0);
        let stored = fixture.store.messages(&fixture.session).unwrap();
        assert!(stored.iter().any(|message| {
            matches!(message.role, Role::Tool)
                && message.tool_call_id.as_deref() == Some("call-2")
                && message.is_error
        }));
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let sent = requests[1]["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call_output" && item["call_id"] == "call-2")
            .expect("the model receives the rejected call's result")["output"]
            .as_str()
            .unwrap();
        for text in diagnostics {
            assert!(sent.contains(text), "{text} missing from {sent}");
        }
        server.abort();
    }
}
