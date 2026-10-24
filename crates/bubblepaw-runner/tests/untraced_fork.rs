//! Without a trace, a summary fork of the root chat still shares the host's
//! prompt-cache ledger: its request continues the root turn's prefix, so the
//! serving log classifies it as an append, not as a first request.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, RunOptions,
    Store, TextCallback, ToolRegistry, Usage,
};
use bubblepaw_runner::{
    episode_models::{
        EpisodeFork, EpisodeModelConfig, EpisodeModels, HttpEpisodeModels, SummaryInput,
    },
    model_roles::ModelRoles,
    provider::ProviderRegistry,
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

/// Records every request; the chat gets a reply, the summary fork gets a memory.
#[derive(Default)]
struct Script(Mutex<Vec<ModelRequest>>);

fn reply(text: &str) -> ModelResponse {
    ModelResponse {
        text: text.into(),
        usage: Some(Usage {
            input_tokens: 12,
            output_tokens: 4,
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[async_trait]
impl Provider for Script {
    fn supports_summary_fork(&self, _: &str) -> bool {
        true
    }
    fn summary_fork_mapping(&self, request: &ModelRequest) -> Result<Option<Value>> {
        Ok(Some(json!({"sources":request.messages.iter()
            .map(|message| json!({"source_id":message.id})).collect::<Vec<_>>()})))
    }
    fn estimate_summary_fork_tokens(
        &self,
        _: &ModelRequest,
        _: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        Ok(Some(10))
    }
    async fn generate_summary_fork(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.0.lock().unwrap().push(request);
        Ok(reply(
            "# Recorded memory\nThe source evidence remains unchanged.\nRelated: none",
        ))
    }
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.0.lock().unwrap().push(request);
        Ok(reply("Acknowledged."))
    }
}

#[tokio::test]
async fn untraced_summary_fork_appends_to_the_root_turn_in_the_serving_log() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("Untraced fork")?.id;
    let provider = Arc::new(Script::default());
    let registry = Arc::new(ProviderRegistry::new("script", provider.clone()));
    registry.set_usage_store(store.clone());
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            model: "script/model".into(),
            system_prompt: "Keep original instructions".into(),
            ..Default::default()
        },
    );
    let run = harness
        .prompt(&session, "Remember the fixture", RunOptions::default())
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);

    let root = provider.0.lock().unwrap()[0].clone();
    assert!(!root.messages.is_empty(), "the root turn sent messages");
    let models = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles: ModelRoles::for_models("script/model".into(), String::new()),
            retry_base_delay: Duration::ZERO,
        },
        registry,
        None,
    )?;
    let fork = EpisodeFork {
        first_message_id: root.messages[0].id.clone(),
        last_message_id: root.messages.last().unwrap().id.clone(),
        request: root,
        provider: provider.clone(),
        max_request_bytes: 1_000_000,
        context_window: Some(1_000_000),
        output_reserve_tokens: 100,
        dispatched: true,
    };
    models
        .summarize_fork(
            SummaryInput {
                session_id: session.clone(),
                focus: None,
                previous_summary: None,
                turn_request: None,
                episode_text: "Canonical evidence".into(),
                cap: 2000,
            },
            fork,
        )
        .await?;

    let rows = store.model_requests(&session)?;
    let root_row = rows
        .iter()
        .find(|row| row.lineage == "root")
        .expect("the root turn writes a row");
    assert_eq!(root_row.classification, "first");
    let fork_row = rows
        .iter()
        .find(|row| row.lineage == "summary_fork")
        .expect("the untraced fork writes a row");
    assert_eq!(
        fork_row.classification, "append",
        "the fork reuses the root prefix of the host's ledger"
    );
    Ok(())
}
