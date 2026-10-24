//! Tools-off rounds on the scripted provider. The first round where tools are
//! unavailable (budget exhausted) keeps the same
//! declarations and queues one reminder. Only a model that still calls a tool
//! gets the enforcement round, which moves the declarations into disabled_tools.
mod support;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, JsonSchema, Message, ModelRequest, ModelResponse,
    Observation, ObservationMode, ObservationRecord, ObservationSink, Provider, Role, RunOptions,
    Store, TextCallback, Tool, ToolCall, ToolContext, ToolDefinition, ToolRegistry, ToolResult,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use support::reply;

const REMINDER: &str = "<harness kind=\"tools_off:";

struct Scripted {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Provider for Scripted {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow!("Unexpected request"))
    }
}

#[derive(Default)]
struct Records(Mutex<Vec<ObservationRecord>>);
impl ObservationSink for Records {
    fn record(&self, record: ObservationRecord) {
        self.0.lock().unwrap().push(record);
    }
}

#[derive(Default)]
struct Counter(AtomicUsize);
#[async_trait]
impl Tool for Counter {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "count".into(),
            description: "count invocations".into(),
            input_schema: json!({"type":"object"}),
            read_only: false,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(ToolResult::text(json!({"ok":true}).to_string()))
    }
}

fn calls(count: usize) -> ModelResponse {
    ModelResponse {
        tool_calls: (0..count)
            .map(|i| ToolCall {
                id: uuid::Uuid::new_v4().to_string(),
                name: "count".into(),
                arguments: json!({"index":i}),
            })
            .collect(),
        ..Default::default()
    }
}
fn submit() -> ModelResponse {
    ModelResponse {
        tool_calls: vec![ToolCall {
            id: uuid::Uuid::new_v4().to_string(),
            name: "submit_result".into(),
            arguments: json!({"count":1}),
        }],
        ..Default::default()
    }
}
fn schema() -> JsonSchema {
    JsonSchema::new(
        "answer",
        json!({
            "type":"object", "additionalProperties":false,
            "properties":{"count":{"type":"integer", "minimum":1}}, "required":["count"]
        }),
    )
    .unwrap()
}

struct Fixture {
    provider: Arc<Scripted>,
    counter: Arc<Counter>,
    records: Arc<Records>,
    store: Store,
    session: String,
    harness: Harness,
}

fn fixture(responses: Vec<ModelResponse>) -> Result<Fixture> {
    let provider = Arc::new(Scripted {
        responses: Mutex::new(responses.into()),
        requests: Mutex::new(vec![]),
    });
    let counter = Arc::new(Counter::default());
    let mut tools = ToolRegistry::new();
    tools.register(counter.clone())?;
    let records = Arc::new(Records::default());
    let store = Store::in_memory()?;
    let session = store.create_session("tools off")?.id;
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        tools,
        HarnessConfig {
            observation: Some(Observation {
                sink: records.clone(),
                mode: ObservationMode::Metadata,
                max_payload_bytes: 0,
            }),
            ..Default::default()
        },
    );
    Ok(Fixture {
        provider,
        counter,
        records,
        store,
        session,
        harness,
    })
}

/// Per root request: classification kind, declared, declaration reason.
fn builds(records: &Records) -> Vec<(String, bool, Option<String>)> {
    records
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|record| record.kind == "context_built")
        .map(|record| {
            (
                record.metrics["classification"]["kind"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                record.metrics["declared"].as_bool().unwrap(),
                record.metrics["declaration_reason"]
                    .as_str()
                    .map(str::to_owned),
            )
        })
        .collect()
}

fn tools_off_reminders(store: &Store, session: &str) -> Result<usize> {
    Ok(store
        .messages(session)?
        .iter()
        .filter(|message| message.content.contains(REMINDER))
        .count())
}

fn tool_results(store: &Store, session: &str) -> Result<Vec<Message>> {
    Ok(store
        .messages(session)?
        .into_iter()
        .filter(|message| message.role == Role::Tool)
        .collect())
}

fn wire(tools: &[ToolDefinition]) -> Result<Value> {
    Ok(serde_json::to_value(tools)?)
}

#[tokio::test]
async fn budget_reminder_complies_with_the_same_declarations_and_appends() -> Result<()> {
    let f = fixture(vec![calls(1), reply("done")])?;
    let run = f
        .harness
        .prompt(
            &f.session,
            "work",
            RunOptions {
                max_tool_calls: Some(1),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(f.counter.0.load(Ordering::Relaxed), 1);
    let requests = f.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(!requests[1].tools.is_empty());
    assert_eq!(wire(&requests[1].tools)?, wire(&requests[0].tools)?);
    assert!(requests[1].disabled_tools.is_empty());
    assert_eq!(tools_off_reminders(&f.store, &f.session)?, 1);
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.content.contains(REMINDER))
    );
    assert!(
        !requests[0]
            .messages
            .iter()
            .any(|m| m.content.contains(REMINDER))
    );
    assert_eq!(
        builds(&f.records),
        [
            ("first".to_owned(), false, None),
            ("append".to_owned(), false, None),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn ignored_budget_reminder_refuses_the_call_then_enforces_tools_off() -> Result<()> {
    let f = fixture(vec![calls(1), calls(1), reply("done")])?;
    let run = f
        .harness
        .prompt(
            &f.session,
            "work",
            RunOptions {
                max_tool_calls: Some(1),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(
        f.counter.0.load(Ordering::Relaxed),
        1,
        "the refused call never ran"
    );
    let requests = f.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(wire(&requests[1].tools)?, wire(&requests[0].tools)?);
    assert!(requests[1].disabled_tools.is_empty());
    assert!(requests[2].tools.is_empty());
    assert_eq!(
        wire(&requests[2].disabled_tools)?,
        wire(&requests[0].tools)?
    );
    assert_eq!(tools_off_reminders(&f.store, &f.session)?, 1);
    let results = tool_results(&f.store, &f.session)?;
    assert_eq!(results.len(), 2);
    assert!(results[1].is_error);
    assert!(results[1].content.contains("budget exhausted"));
    assert!(results[1].content.contains("not executed"));
    assert_eq!(
        builds(&f.records),
        [
            ("first".to_owned(), false, None),
            ("append".to_owned(), false, None),
            (
                "frozen_change".to_owned(),
                true,
                Some("tools_off_enforced".to_owned())
            ),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn result_nudge_keeps_the_same_declarations_and_appends() -> Result<()> {
    let f = fixture(vec![reply("bad"), submit()])?;
    let run = f
        .harness
        .prompt(
            &f.session,
            "give JSON",
            RunOptions {
                output_schema: Some(schema()),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(run.text.as_deref(), Some(r#"{"count":1}"#));
    let requests = f.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(!requests[1].tools.is_empty());
    assert_eq!(wire(&requests[1].tools)?, wire(&requests[0].tools)?);
    assert!(requests[1].disabled_tools.is_empty());
    assert_eq!(tools_off_reminders(&f.store, &f.session)?, 0);
    assert_eq!(
        builds(&f.records),
        [
            ("first".to_owned(), false, None),
            ("append".to_owned(), false, None),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn unrelated_calls_after_result_nudge_stay_available() -> Result<()> {
    let f = fixture(vec![reply("bad"), calls(1), submit()])?;
    let run = f
        .harness
        .prompt(
            &f.session,
            "give JSON",
            RunOptions {
                output_schema: Some(schema()),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(f.counter.0.load(Ordering::Relaxed), 1);
    let requests = f.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for request in requests.iter() {
        assert_eq!(wire(&request.tools)?, wire(&requests[0].tools)?);
        assert!(request.disabled_tools.is_empty());
        assert!(request.output_schema.is_none());
    }
    assert_eq!(tools_off_reminders(&f.store, &f.session)?, 0);
    assert!(
        tool_results(&f.store, &f.session)?
            .iter()
            .all(|result| !result.is_error)
    );
    Ok(())
}
