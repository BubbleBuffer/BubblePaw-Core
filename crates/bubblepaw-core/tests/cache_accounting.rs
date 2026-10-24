//! Scripted-model replay of the main loop through prompt-cache accounting:
//! every step must be a pure append, and the only prefix break must be the
//! one the context declared.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextPreparation, Harness, HarnessConfig, Message, ModelRequest,
    ModelResponse, Observation, ObservationMode, ObservationRecord, ObservationSink, Provider,
    RunOptions, Store, TextCallback, Tool, ToolCall, ToolContext, ToolDefinition, ToolRegistry,
    ToolResult, Usage, WireShape,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

/// A Chat-like wire body, as an adapter would send it.
fn body(request: &ModelRequest) -> Value {
    let mut messages = vec![json!({"role":"system","content":request.system_prompt})];
    messages.extend(request.messages.iter().map(|m| {
        json!({"role":m.role,"content":m.content,"tool_calls":m.tool_calls,"tool_call_id":m.tool_call_id})
    }));
    json!({"model":request.model,"messages":messages,"tools":request.tools,"max_tokens":request.max_output_tokens})
}

/// Reports a perfect provider cache: every request reads the previous one.
struct Replay {
    responses: Mutex<VecDeque<ModelResponse>>,
    previous_input: AtomicU64,
}
#[async_trait]
impl Provider for Replay {
    fn wire_shape(&self, request: &ModelRequest) -> Option<WireShape> {
        Some(WireShape {
            provider: "scripted".into(),
            model: request.model.clone(),
            ..WireShape::from_body(&body(request), "messages", 64)
        })
    }
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let input = body(&request).to_string().len() as u64 / 4;
        let read = self.previous_input.swap(input, Ordering::SeqCst);
        let mut response = self.responses.lock().unwrap().pop_front().unwrap();
        response.usage = Some(Usage {
            input_tokens: input,
            output_tokens: 10,
            cache_read_tokens: read,
            ..Usage::default()
        });
        Ok(response)
    }
}

struct Echo;
#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            approval: Default::default(),
            name: "echo".into(),
            description: "Echo".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
        }
    }
    async fn invoke(&self, _: ToolContext, input: Value) -> Result<ToolResult> {
        Ok(ToolResult {
            content: input.to_string(),
            is_error: false,
            pending: false,
        })
    }
}

/// Once `apply` is set, serves history without its opening message, like a
/// replacement prefix; declares it only when `declare` is true.
struct Apply {
    apply: AtomicBool,
    pending: AtomicBool,
    declare: bool,
}
impl ContextPreparation for Apply {
    fn project_history(&self, history: &[Message]) -> Result<Vec<Message>> {
        Ok(if self.apply.load(Ordering::SeqCst) {
            let mut view = history[0].clone();
            view.role = bubblepaw_core::Role::System;
            view.content = "compacted view".into();
            let mut messages = vec![view];
            messages.extend_from_slice(&history[1..]);
            messages
        } else {
            history.to_vec()
        })
    }
    fn cold_message_indexes(&self, _: &str, _: &ModelRequest) -> Result<Vec<usize>> {
        Ok(if self.apply.load(Ordering::SeqCst) {
            vec![0]
        } else {
            vec![]
        })
    }
    fn take_prefix_change(&self, _: &str) -> Option<String> {
        (self.declare && self.pending.swap(false, Ordering::SeqCst)).then(|| "lcm_apply".into())
    }
}

#[derive(Default)]
struct Records(Mutex<Vec<ObservationRecord>>);
impl ObservationSink for Records {
    fn record(&self, record: ObservationRecord) {
        self.0.lock().unwrap().push(record);
    }
}

fn tool_step(id: &str) -> ModelResponse {
    ModelResponse {
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "echo".into(),
            arguments: json!({"step":id}),
        }],
        ..Default::default()
    }
}
fn answer(text: &str) -> ModelResponse {
    ModelResponse {
        text: text.into(),
        ..Default::default()
    }
}

async fn replay(declare: bool) -> Result<Vec<ObservationRecord>> {
    let store = Store::in_memory()?;
    let session = store.create_session("cache replay")?;
    let provider = Arc::new(Replay {
        responses: Mutex::new(
            vec![
                tool_step("a"),
                tool_step("b"),
                answer("one"),
                tool_step("c"),
                answer("two"),
                tool_step("d"),
                answer("three"),
            ]
            .into(),
        ),
        previous_input: AtomicU64::new(0),
    });
    let context = Arc::new(Apply {
        apply: AtomicBool::new(false),
        pending: AtomicBool::new(false),
        declare,
    });
    let records = Arc::new(Records::default());
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Echo))?;
    let agent = Harness::new(
        store,
        provider,
        tools,
        HarnessConfig {
            context: context.clone(),
            observation: Some(Observation {
                sink: records.clone(),
                mode: ObservationMode::Metadata,
                max_payload_bytes: 0,
            }),
            ..Default::default()
        },
    );
    for prompt in ["first", "second"] {
        let run = agent
            .prompt(&session.id, prompt, RunOptions::default())
            .await?;
        assert_eq!(run.status, "completed", "{:?}", run.error);
    }
    context.apply.store(true, Ordering::SeqCst);
    context.pending.store(true, Ordering::SeqCst);
    let run = agent
        .prompt(&session.id, "third", RunOptions::default())
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    Ok(records.0.lock().unwrap().clone())
}

#[tokio::test]
async fn main_loop_is_append_only_and_declared_bust_is_the_only_break() -> Result<()> {
    let records = replay(true).await?;
    assert_context_sequence(&records, true);
    let finished: Vec<_> = records
        .iter()
        .filter(|r| r.kind == "model_finished")
        .collect();
    assert_eq!(finished.len(), 7);
    let busts: Vec<_> = records.iter().filter(|r| r.kind == "cache_bust").collect();
    assert_eq!(busts.len(), 1, "exactly one prefix break");
    assert_eq!(busts[0].metrics["declared"], true);
    assert_eq!(busts[0].metrics["reason"], "lcm_apply");
    for (index, record) in finished.iter().enumerate() {
        let metrics = &record.metrics;
        assert_eq!(metrics["cache_group"]["provider"], "scripted");
        assert_eq!(metrics["cache_group"]["kind"], "main");
        if index == 5 {
            assert_eq!(metrics["prefix_break"], "lcm_apply");
            continue;
        }
        assert!(metrics["prefix_break"].is_null(), "step {index} appended");
        // A perfect cache reads at least the expected block-rounded prefix.
        assert_eq!(metrics["unexplained_miss_tokens"], 0, "step {index}");
        if index > 0 {
            assert!(metrics["expected_cacheable_tokens"].as_u64().unwrap() > 0);
        }
    }
    let undeclared = replay(false).await?;
    assert_context_sequence(&undeclared, false);
    let busts: Vec<_> = undeclared
        .iter()
        .filter(|r| r.kind == "cache_bust")
        .collect();
    assert_eq!(busts.len(), 1);
    assert_eq!(busts[0].metrics["declared"], false);
    assert_eq!(busts[0].metrics["reason"], "undeclared");
    Ok(())
}

fn assert_context_sequence(records: &[ObservationRecord], declared: bool) {
    let builds: Vec<_> = records
        .iter()
        .filter(|record| record.kind == "context_built")
        .collect();
    let kinds: Vec<_> = builds
        .iter()
        .map(|record| record.metrics["classification"]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "first",
            "append",
            "append",
            "append",
            "append",
            "cold_change",
            "append"
        ]
    );
    for (index, record) in builds.iter().enumerate() {
        assert_eq!(record.metrics["request_kind"], "root");
        assert_eq!(record.metrics["declared"], index == 5 && declared);
        for tier in ["frozen", "cold", "history"] {
            assert_eq!(record.metrics[tier]["sha256"].as_str().unwrap().len(), 64);
            assert!(record.metrics[tier]["bytes"].as_u64().unwrap() > 0);
        }
        assert!(record.payload.is_none());
        let started = records
            .iter()
            .position(|candidate| {
                candidate.kind == "model_started"
                    && candidate.scope.model_request_id == record.scope.model_request_id
            })
            .unwrap();
        let built = records
            .iter()
            .position(|candidate| std::ptr::eq(candidate, *record))
            .unwrap();
        assert!(
            built < started,
            "context must be built before model dispatch"
        );
    }
}
