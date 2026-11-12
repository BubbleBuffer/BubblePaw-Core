//! Templates preview an unbound chat, then read its persisted prefix without
//! rebinding it or observing changes in live host configuration.
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Observation,
    ObservationMode, ObservationRecord, ObservationSink, Provider, RunOptions, Store, TextCallback,
    Tool, ToolContext, ToolDefinition, ToolRegistry, ToolResult,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Answer(Mutex<Vec<ModelRequest>>);
#[async_trait]
impl Provider for Answer {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.0.lock().unwrap().push(request);
        Ok(ModelResponse {
            text: "ok".into(),
            ..Default::default()
        })
    }
}

#[derive(Default)]
struct Records(Mutex<Vec<ObservationRecord>>);
impl ObservationSink for Records {
    fn record(&self, record: ObservationRecord) {
        self.0.lock().unwrap().push(record);
    }
}

struct Named(&'static str);
#[async_trait]
impl Tool for Named {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            approval: Default::default(),
            name: self.0.into(),
            description: self.0.into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        Ok(ToolResult {
            content: "ok".into(),
            is_error: false,
            pending: false,
        })
    }
}

/// A registry with one tool and the given publish generation.
fn registry(tool: &'static str, generation: u64) -> Result<ToolRegistry> {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Named(tool)))?;
    Ok(tools.with_generation(generation))
}

#[tokio::test]
async fn templates_preview_live_configuration_until_admission_then_read_the_chat_prefix()
-> Result<()> {
    let prompt = Arc::new(Mutex::new("A".to_owned()));
    let source = Arc::new(Mutex::new(registry("echo", 1)?));
    let records = Arc::new(Records::default());
    let store = Store::in_memory()?;
    let session = store.create_session("request template")?.id;
    let resolver_prompt = prompt.clone();
    let resolver_source = source.clone();
    let provider = Arc::new(Answer::default());
    let harness = Harness::new(
        store,
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            system_prompt: "base".into(),
            system_prompt_resolver: Some(Arc::new(move |_: &str, base: &str| -> Result<String> {
                Ok(format!("{base} {}", resolver_prompt.lock().unwrap()))
            })),
            registry_resolver: Some(Arc::new(move || resolver_source.lock().unwrap().clone())),
            observation: Some(Observation {
                sink: records.clone(),
                mode: ObservationMode::Metadata,
                max_payload_bytes: 0,
            }),
            ..Default::default()
        },
    );
    let initial_preview = harness.request_template(&session)?;
    assert_eq!(initial_preview.system_prompt, "base A");
    assert!(initial_preview.tools.iter().any(|tool| tool.name == "echo"));
    *prompt.lock().unwrap() = "B".into();
    *source.lock().unwrap() = registry("search", 2)?;
    let template = harness.request_template(&session)?;
    assert_eq!(template.system_prompt, "base B");
    let names: Vec<_> = template
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(names, ["call_tool", "search", "tool_search"]);
    assert!(template.messages.is_empty());
    assert!(
        records.0.lock().unwrap().is_empty(),
        "previews do not observe or bind a request"
    );
    let first = harness
        .prompt(&session, "one", RunOptions::default())
        .await?;
    assert_eq!(first.status, "completed");

    *prompt.lock().unwrap() = "C".into();
    *source.lock().unwrap() = registry("later", 3)?;
    let before_preview = records.0.lock().unwrap().len();
    let saved = harness.request_template(&session)?;
    assert_eq!(saved.system_prompt, template.system_prompt);
    assert_eq!(
        serde_json::to_vec(&saved.tools)?,
        serde_json::to_vec(&template.tools)?
    );
    assert_eq!(records.0.lock().unwrap().len(), before_preview);
    let run = harness
        .prompt(&session, "two", RunOptions::default())
        .await?;
    if run.status != "completed" {
        return Err(anyhow!("run ended {}", run.status));
    }
    let builds: Vec<_> = records
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|record| record.kind == "context_built")
        .map(|record| record.metrics.clone())
        .collect();
    let kinds: Vec<_> = builds
        .iter()
        .map(|build| build["classification"]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["first", "append"]);
    let requests = provider.0.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(request.system_prompt, template.system_prompt);
        assert_eq!(
            serde_json::to_vec(&request.tools)?,
            serde_json::to_vec(&template.tools)?
        );
    }
    Ok(())
}
