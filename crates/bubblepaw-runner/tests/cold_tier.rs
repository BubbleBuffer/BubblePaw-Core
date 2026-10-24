//! P4 acceptance: an LCM apply changes only the cold tier. The system prompt and
//! tool declarations stay byte-identical, the runtime facts reach the model once,
//! as the runtime_inventory reminder, and the lookup hint is a cold note.
//! P5: the Messages wire `system` stays byte-identical too; the view and the
//! cold note render as user content.
use anyhow::{Result, bail};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextStrategy, Harness, HarnessConfig, ModelRequest, ModelResponse,
    Observation, ObservationMode, ObservationRecord, ObservationSink, Provider, Role, RunOptions,
    Store, TextCallback, ToolRegistry, Usage,
};
use bubblepaw_runner::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    episode_models::{
        ChapterInput, ChapterResult, EpisodeModels, SplitInput, SplitResult, SummaryInput,
        SummaryResult,
    },
    episode_worker::EpisodeWorker,
    episodes::{EpisodeConfig, EpisodeSummaryInput},
    provider::MessagesProvider,
    runtime_note::RuntimeNotes,
    summarization::{HISTORY_RETRIEVAL_INSTRUCTION, SummarizingContext, SummaryConfig},
};
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Main(Mutex<Vec<ModelRequest>>);
#[async_trait]
impl Provider for Main {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.0.lock().unwrap().push(request);
        Ok(ModelResponse {
            text: "Scripted answer".into(),
            ..Default::default()
        })
    }
}

struct Models;
#[async_trait]
impl EpisodeModels for Models {
    async fn decide(&self, input: SplitInput) -> Result<SplitResult> {
        let mut probabilities = vec![0.0; input.options.len() + 1];
        *probabilities.last_mut().unwrap() = 1.0;
        Ok(SplitResult {
            choice: None,
            probabilities,
            model: "scripted".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
    async fn summarize(&self, _: SummaryInput) -> Result<SummaryResult> {
        Ok(SummaryResult {
            title: "Retained facts".into(),
            summary: "# Retained facts\nOriginal decisions retained.".into(),
            model: "scripted".into(),
            usage: Usage::default(),
            reasoning: None,
        })
    }
    async fn close_chapter(&self, _: ChapterInput) -> Result<ChapterResult> {
        bail!("no subject changes in this fixture")
    }
}

#[derive(Default)]
struct Sink(Mutex<Vec<ObservationRecord>>);
impl ObservationSink for Sink {
    fn record(&self, record: ObservationRecord) {
        self.0.lock().unwrap().push(record);
    }
}

fn frozen(request: &ModelRequest) -> Value {
    serde_json::json!({"system": request.system_prompt, "tools": request.tools})
}

/// The Messages wire input (system, messages, tools) for a captured request.
fn messages_wire(request: &ModelRequest) -> Result<Value> {
    let provider = MessagesProvider::api_key("synthetic-key", "http://127.0.0.1:1")?;
    let wire = Mutex::new(Value::Null);
    provider.estimate_input_tokens(request, &|input| {
        *wire.lock().unwrap() = input.clone();
        0
    })?;
    Ok(wire.into_inner().unwrap())
}

#[tokio::test]
async fn lcm_apply_changes_only_the_cold_tier() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let session = store
        .create_session_with_strategy("Cold tier", ContextStrategy::Lcm)?
        .id;
    let artifacts = Arc::new(ArtifactStore::new(
        directory.path(),
        directory.path().join("artifacts"),
    )?);
    let repl = Arc::new(CodeMode::new(directory.path().to_owned(), artifacts)?);
    let notes = Arc::new(RuntimeNotes::without_processes(repl.clone()));
    let main = Arc::new(Main::default());
    let context = Arc::new(
        SummarizingContext::new(
            store.clone(),
            main.clone(),
            SummaryConfig::default(),
            Some(notes),
        )?
        .with_auto_summarize(false),
    );
    let worker = EpisodeWorker::new_with_config(
        store.clone(),
        Arc::new(Models),
        context.clone(),
        EpisodeConfig {
            summary_input: EpisodeSummaryInput::Episode,
            ..Default::default()
        },
    )?;
    context.set_episode_worker(&worker);
    let mut tools = ToolRegistry::new();
    repl.register(&mut tools)?;
    let sink = Arc::new(Sink::default());
    let harness = Harness::new(
        store.clone(),
        main.clone(),
        tools,
        HarnessConfig {
            model: "scripted".into(),
            system_prompt: "Keep original instructions".into(),
            context: context.clone(),
            observation: Some(Observation {
                sink: sink.clone(),
                mode: ObservationMode::Metadata,
                max_payload_bytes: 0,
            }),
            ..Default::default()
        },
    );
    for index in 0..3 {
        let run = harness
            .prompt(
                &session,
                &format!("Exact request {index}"),
                RunOptions::default(),
            )
            .await?;
        assert_eq!(run.status, "completed", "{:?}", run.error);
    }
    harness.compact_session(&session, None).await?;
    assert!(
        store.context_view(&session)?.is_some(),
        "the view is applied"
    );
    let run = harness
        .prompt(&session, "After the apply", RunOptions::default())
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);

    let requests = main.0.lock().unwrap().clone();
    let (before, after) = (&requests[requests.len() - 2], requests.last().unwrap());
    assert!(!after.tools.is_empty(), "the fixture declares tools");
    assert_eq!(
        serde_json::to_vec(&frozen(after))?,
        serde_json::to_vec(&frozen(before))?,
        "system prompt and tools are byte-identical across the apply"
    );
    let build = sink
        .0
        .lock()
        .unwrap()
        .iter()
        .rfind(|record| record.kind == "context_built" && record.scope.run_id == run.id)
        .expect("the post-apply run builds a request")
        .metrics
        .clone();
    assert_eq!(build["classification"]["kind"], "cold_change", "{build}");
    assert_eq!(build["declared"], true, "{build}");
    assert_eq!(build["declaration_reason"], "lcm_apply", "{build}");
    assert!(build["cold"]["message_count"].as_u64() > Some(1), "{build}");

    let body = after
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!body.contains("[host notice]"), "no inventory in the view");
    assert_eq!(
        body.matches("<harness kind=\"runtime_inventory\">").count(),
        1,
        "the runtime facts reach the model exactly once"
    );
    assert!(body.contains(bubblepaw_core::RUNTIME_INVENTORY_HEADER));
    let lookup = after
        .messages
        .iter()
        .position(|message| {
            message
                .content
                .contains("<harness kind=\"history_lookup\">")
        })
        .expect("the lookup hint is a cold note");
    assert!(
        after.messages[lookup]
            .content
            .contains(HISTORY_RETRIEVAL_INSTRUCTION)
    );
    assert!(
        after
            .messages
            .iter()
            .all(|message| message.content != HISTORY_RETRIEVAL_INSTRUCTION),
        "the view prefix no longer carries the lookup hint itself"
    );
    assert!(
        after.messages[lookup + 1..]
            .iter()
            .all(|message| !message.content.starts_with("Closed chapter")
                && !message.content.starts_with("Open chapter")),
        "the note follows the view"
    );

    let (wire_before, wire_after) = (messages_wire(before)?, messages_wire(after)?);
    assert_eq!(
        serde_json::to_vec(&wire_after["system"])?,
        serde_json::to_vec(&wire_before["system"])?,
        "the Messages system field is byte-identical across the apply"
    );
    assert_eq!(wire_after["system"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        wire_after["system"][0]["text"],
        after.system_prompt.as_str()
    );
    let user_text: Vec<&str> = wire_after["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|turn| turn["role"] == "user")
        .flat_map(|turn| turn["content"].as_array().unwrap())
        .filter_map(|block| block["text"].as_str())
        .collect();
    let system: Vec<_> = after
        .messages
        .iter()
        .filter(|message| message.role == Role::System)
        .collect();
    assert!(system.len() > 1, "view blocks and the cold note");
    for message in system {
        assert!(user_text.contains(&message.content.as_str()), "{message:?}");
    }
    Ok(())
}
