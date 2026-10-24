mod support;

use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextCheckpoint, ContextPreparation, EventKind, Harness, HarnessConfig,
    Message, ModelRequest, ModelResponse, PrepareCall, PreparedContext, Provider, RunOptions,
    Store, TextCallback, ToolRegistry, Usage,
};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

#[derive(Default)]
struct RecordingProvider(Mutex<Vec<ModelRequest>>);
#[async_trait]
impl Provider for RecordingProvider {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.0.lock().unwrap().push(request);
        Ok(ModelResponse {
            text: "answer".into(),
            usage: Some(Usage {
                input_tokens: 20,
                output_tokens: 3,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}

fn harness(store: &Store) -> Harness {
    Harness::new(
        store.clone(),
        Arc::new(RecordingProvider::default()),
        ToolRegistry::new(),
        HarnessConfig::default(),
    )
}

fn checkpoint(session: &str, message: &Message, name: &str) -> ContextCheckpoint {
    ContextCheckpoint {
        files_touched: Vec::new(),
        id: name.into(),
        session_id: session.into(),
        through_message_id: message.id.clone(),
        model: "summary-model".into(),
        summary: "Preserved decisions.".into(),
        created_at: message.created_at.clone(),
        usage: Some(Usage {
            input_tokens: 10,
            output_tokens: 2,
            ..Default::default()
        }),
    }
}

#[tokio::test]
async fn checkpoint_restart_reads_and_fork_preserve_the_original_archive() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("archive.sqlite");
    let store = Store::open(&path)?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, 0);
    let session = store.create_session("checkpoint")?;
    let agent = harness(&store);
    agent
        .prompt(&session.id, "Original user input", RunOptions::default())
        .await?;
    let original = serde_json::to_value(store.messages(&session.id)?)?;
    let messages = store.messages(&session.id)?;
    let before = store.storage_stats()?.chat_transaction_count;
    assert!(store.context_checkpoint(&session.id)?.is_none());
    assert_eq!(store.storage_stats()?.chat_transaction_count, before);
    let first = checkpoint(&session.id, &messages[0], "first");
    store.save_context_checkpoint(&first)?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, before + 1);
    let latest = checkpoint(&session.id, &messages[1], "latest");
    store.save_context_checkpoint(&latest)?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, before + 2);
    for _ in 0..5 {
        assert_eq!(store.context_checkpoint(&session.id)?.unwrap().id, "latest");
    }
    assert_eq!(store.storage_stats()?.chat_transaction_count, before + 2);
    assert_eq!(
        serde_json::to_value(store.messages(&session.id)?)?,
        original
    );
    let fork = store.fork(&session.id, "fork")?;
    assert!(store.context_checkpoint(&fork.id)?.is_none());
    assert_ne!(store.messages(&fork.id)?[0].id, messages[0].id);
    drop(agent);
    drop(store);
    let reopened = Store::open(&path)?;
    assert_eq!(reopened.storage_stats()?.chat_transaction_count, 0);
    assert_eq!(
        serde_json::to_value(reopened.messages(&session.id)?)?,
        original
    );
    assert_eq!(
        serde_json::to_value(reopened.context_checkpoint(&session.id)?.unwrap())?,
        serde_json::to_value(latest)?
    );
    let conn = rusqlite::Connection::open(reopened.chat_path(&session.id)?.unwrap())?;
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM context_checkpoints", [], |r| r
            .get::<_, i64>(0))?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn invalid_foreign_and_regressing_cutoffs_never_replace_a_checkpoint() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("source")?;
    let foreign = store.create_session("foreign")?;
    let agent = harness(&store);
    agent
        .prompt(&session.id, "one", RunOptions::default())
        .await?;
    agent
        .prompt(&foreign.id, "other", RunOptions::default())
        .await?;
    let messages = store.messages(&session.id)?;
    let valid = checkpoint(&session.id, &messages[1], "valid");
    store.save_context_checkpoint(&valid)?;
    let before = store.storage_stats()?.chat_transaction_count;
    let mut bad = valid.clone();
    bad.through_message_id = messages[0].id.clone();
    assert!(store.save_context_checkpoint(&bad).is_err());
    bad.through_message_id = store.messages(&foreign.id)?[1].id.clone();
    assert!(store.save_context_checkpoint(&bad).is_err());
    bad.through_message_id = "missing".into();
    assert!(store.save_context_checkpoint(&bad).is_err());
    bad = valid.clone();
    bad.summary = " ".into();
    assert!(store.save_context_checkpoint(&bad).is_err());
    bad.summary = "x".repeat(65537);
    assert!(store.save_context_checkpoint(&bad).is_err());
    bad = valid.clone();
    bad.id.clear();
    assert!(store.save_context_checkpoint(&bad).is_err());
    bad = valid.clone();
    bad.model.clear();
    assert!(store.save_context_checkpoint(&bad).is_err());
    bad = valid.clone();
    bad.session_id = "missing".into();
    assert!(store.save_context_checkpoint(&bad).is_err());
    assert_eq!(store.storage_stats()?.chat_transaction_count, before);
    assert_eq!(
        serde_json::to_value(store.context_checkpoint(&session.id)?.unwrap())?,
        serde_json::to_value(valid)?
    );
    assert!(store.context_checkpoint(&foreign.id)?.is_none());
    Ok(())
}

#[tokio::test]
async fn existing_v1_and_v2_archives_migrate_without_rewriting_messages() -> Result<()> {
    for version in [1, 2] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("archive.sqlite");
        let store = Store::open(&path)?;
        let session = store.create_session("old archive")?;
        let agent = harness(&store);
        agent
            .prompt(&session.id, "retained", RunOptions::default())
            .await?;
        let original = serde_json::to_value(store.messages(&session.id)?)?;
        drop(agent);
        drop(store);
        support::replace_with_legacy_fixture(&path, version)?;
        let conn = rusqlite::Connection::open(&path)?;
        let insert_effect = if version == 1 {
            "INSERT INTO effects(id,session_id,run_id,call_id,state) VALUES('retained-effect',?1,'old-run','old-call','completed')"
        } else {
            "INSERT INTO effects(id,session_id,run_id,call_id,state,detail) VALUES('retained-effect',?1,'old-run','old-call','completed','retained detail')"
        };
        conn.execute(insert_effect, [&session.id])?;
        if version == 2 {
            let output = serde_json::json!({
                "id":"retained-output", "session_id":session.id, "run_id":"old-run",
                "execution_id":"old-execution", "created_at":"2026-10-06T00:00:00Z",
                "output":{"type":"text","text":"retained output"}
            });
            conn.execute(
                "INSERT INTO outputs(id,session_id,execution_id,data) VALUES('retained-output',?1,'old-execution',?2)",
                rusqlite::params![session.id, serde_json::to_string(&output)?],
            )?;
        }
        drop(conn);
        let store = Store::open(&path)?;
        assert_eq!(store.storage_stats()?.chat_transaction_count, 0);
        assert_eq!(
            serde_json::to_value(store.messages(&session.id)?)?,
            original
        );
        let messages = store.messages(&session.id)?;
        store.save_context_checkpoint(&checkpoint(&session.id, &messages[1], "migrated"))?;
        let conn = rusqlite::Connection::open(store.chat_path(&session.id)?.unwrap())?;
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?,
            11
        );
        assert_eq!(
            conn.query_row(
                "SELECT state FROM effects WHERE id='retained-effect'",
                [],
                |r| r.get::<_, String>(0)
            )?,
            "completed"
        );
        if version == 2 {
            assert_eq!(
                rusqlite::Connection::open(&path)?.query_row(
                    "SELECT detail FROM effects WHERE id='retained-effect'",
                    [],
                    |r| r.get::<_, String>(0)
                )?,
                "retained detail"
            );
            assert!(
                conn.query_row(
                    "SELECT detail FROM effects WHERE id='retained-effect'",
                    [],
                    |r| r.get::<_, Option<String>>(0)
                )?
                .is_none()
            );
            assert_eq!(
                store.outputs(&session.id, 20)?[0].output["text"],
                "retained output"
            );
        }
    }
    Ok(())
}

struct LegacyHook;
impl ContextPreparation for LegacyHook {
    fn project_history(&self, history: &[Message]) -> Result<Vec<Message>> {
        Ok(history.last().cloned().into_iter().collect())
    }
}

#[tokio::test]
async fn synchronous_hook_requires_no_async_implementation() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("legacy")?;
    let provider = Arc::new(RecordingProvider::default());
    let agent = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            context: Arc::new(LegacyHook),
            ..Default::default()
        },
    );
    agent
        .prompt(&session.id, "one", RunOptions::default())
        .await?;
    agent
        .prompt(&session.id, "two", RunOptions::default())
        .await?;
    assert_eq!(provider.0.lock().unwrap()[1].messages.len(), 1);
    assert_eq!(store.messages(&session.id)?.len(), 4);
    assert!(store.outputs(&session.id, 20)?.is_empty());
    Ok(())
}

#[derive(Default)]
struct AsyncHook {
    observed: Mutex<Vec<u64>>,
}
#[async_trait]
impl ContextPreparation for AsyncHook {
    async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
        let mut request = call.request;
        // The archive is deliberately too big for the final request guard.
        request.messages.clear();
        Ok(PreparedContext {
            request,
            notice: Some("Earlier context summarized.".into()),
            usage: Some(Usage {
                input_tokens: 100,
                output_tokens: 10,
                ..Default::default()
            }),
        })
    }
    fn observe_response(&self, _: &str, response: &ModelResponse) {
        self.observed
            .lock()
            .unwrap()
            .push(response.usage.as_ref().unwrap().input_tokens);
    }
}

#[tokio::test]
async fn asynchronous_preparation_checks_final_view_persists_notice_and_accounts_usage()
-> Result<()> {
    // Leave room for native discovery; raw history must still exceed the cap.
    let max_request_bytes = 4096;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("notice.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("async")?;
    let provider = Arc::new(RecordingProvider::default());
    let policy = Arc::new(AsyncHook::default());
    let agent = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            context: policy.clone(),
            max_request_bytes,
            ..Default::default()
        },
    );
    let mut events = agent.subscribe();
    let before = store.storage_stats()?.chat_transaction_count;
    let run = agent
        .prompt(
            &session.id,
            "x".repeat(max_request_bytes),
            RunOptions::default(),
        )
        .await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert_eq!(run.usage.as_ref().unwrap().input_tokens, 120);
    assert_eq!(run.usage.unwrap().output_tokens, 13);
    assert_eq!(*policy.observed.lock().unwrap(), vec![20]);
    let prepared = provider.0.lock().unwrap()[0].clone();
    assert!(prepared.messages.is_empty());
    assert!(serde_json::to_vec(&prepared)?.len() < max_request_bytes);
    let mut unprepared = prepared;
    unprepared.messages = vec![store.messages(&session.id)?[0].clone()];
    assert!(serde_json::to_vec(&unprepared)?.len() > max_request_bytes);
    // Admission, notice and terminal reply. Resolved input/run metadata
    // leaves the record within the owning transaction before serving flush.
    assert_eq!(store.storage_stats()?.chat_transaction_count - before, 3);
    assert_eq!(store.messages(&session.id)?.len(), 2);
    assert!(
        !store
            .messages(&session.id)?
            .iter()
            .any(|m| m.content == "Earlier context summarized.")
    );
    let outputs = store.outputs(&session.id, 20)?;
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].run_id, run.id);
    assert!(outputs[0].execution_id.starts_with("context:"));
    assert_eq!(outputs[0].output["text"], "Earlier context summarized.");
    let mut notice_paragraphs = 0;
    while let Ok(event) = events.try_recv() {
        if let EventKind::Paragraph { text } = event.kind
            && text == "Earlier context summarized."
        {
            notice_paragraphs += 1;
        }
    }
    assert_eq!(notice_paragraphs, 1);
    drop(agent);
    drop(store);
    let reopened = Store::open(&path)?;
    assert_eq!(
        reopened.outputs(&session.id, 20)?[0].output,
        outputs[0].output
    );
    assert_eq!(reopened.messages(&session.id)?.len(), 2);
    Ok(())
}

struct WaitingHook(Arc<Notify>);
#[async_trait]
impl ContextPreparation for WaitingHook {
    async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
        self.0.notify_one();
        call.cancellation.cancelled().await;
        Ok(PreparedContext {
            request: call.request,
            notice: Some("late notice".into()),
            usage: None,
        })
    }
}

#[tokio::test]
async fn cancelling_preparation_prevents_late_notice_and_provider_dispatch() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("cancel")?;
    let entered = Arc::new(Notify::new());
    let provider = Arc::new(RecordingProvider::default());
    let agent = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            context: Arc::new(WaitingHook(entered.clone())),
            ..Default::default()
        },
    );
    let mut events = agent.subscribe();
    agent.start(&session.id, "retained input", RunOptions::default())?;
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified()).await?;
    assert!(agent.cancel(&session.id));
    let run = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        agent.wait_for_idle(&session.id),
    )
    .await??;
    assert_eq!(run.status, "cancelled");
    assert!(provider.0.lock().unwrap().is_empty());
    assert!(store.outputs(&session.id, 20)?.is_empty());
    assert_eq!(store.messages(&session.id)?[0].content, "retained input");
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(
            event.kind,
            EventKind::Paragraph { .. } | EventKind::Output { .. }
        ));
    }
    Ok(())
}

struct FailedPreparation;
#[async_trait]
impl ContextPreparation for FailedPreparation {
    async fn prepare(&self, _: PrepareCall<'_>) -> Result<PreparedContext> {
        anyhow::bail!("summary request failed")
    }
}

struct FailedProvider;
#[async_trait]
impl Provider for FailedProvider {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        anyhow::bail!("provider request failed")
    }
}

#[tokio::test]
async fn preparation_and_provider_failures_preserve_existing_history_and_checkpoint() -> Result<()>
{
    let store = Store::in_memory()?;
    let session = store.create_session("failures")?;
    let setup = harness(&store);
    setup
        .prompt(&session.id, "archived input", RunOptions::default())
        .await?;
    let original = store.messages(&session.id)?;
    let saved = checkpoint(&session.id, &original[1], "saved");
    store.save_context_checkpoint(&saved)?;
    let provider = Arc::new(RecordingProvider::default());
    let failed_preparation = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            context: Arc::new(FailedPreparation),
            ..Default::default()
        },
    );
    let run = failed_preparation
        .resume(&session.id, RunOptions::default())
        .await?;
    assert_eq!(run.status, "failed");
    assert!(run.error.unwrap().contains("summary request failed"));
    assert!(provider.0.lock().unwrap().is_empty());
    let failed_provider = Harness::new(
        store.clone(),
        Arc::new(FailedProvider),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    let run = failed_provider
        .resume(&session.id, RunOptions::default())
        .await?;
    assert_eq!(run.status, "failed");
    assert!(run.error.unwrap().contains("provider request failed"));
    assert_eq!(
        serde_json::to_value(store.messages(&session.id)?)?,
        serde_json::to_value(original)?
    );
    assert_eq!(
        serde_json::to_value(store.context_checkpoint(&session.id)?.unwrap())?,
        serde_json::to_value(saved)?
    );
    assert!(store.outputs(&session.id, 20)?.is_empty());
    Ok(())
}
