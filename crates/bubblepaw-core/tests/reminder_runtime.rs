use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bubblepaw_core::*;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

struct Capture {
    requests: Mutex<Vec<ModelRequest>>,
    replies: Mutex<VecDeque<ModelResponse>>,
}
impl Capture {
    fn new(replies: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(vec![]),
            replies: Mutex::new(
                replies
                    .iter()
                    .map(|text| ModelResponse {
                        text: (*text).into(),
                        ..Default::default()
                    })
                    .collect(),
            ),
        })
    }
}
#[async_trait]
impl Provider for Capture {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow!("Unexpected model call"))
    }
}
struct Notices(Mutex<Vec<Reminder>>);
#[async_trait]
impl Tool for Notices {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "notice_source".into(),
            description: "notices".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Default::default(),
        }
    }
    async fn invoke(&self, _: ToolContext, _: Value) -> Result<ToolResult> {
        Ok(ToolResult::text("done"))
    }
    fn take_reminders(&self, _: &str) -> Vec<Reminder> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}
struct FailOnce(AtomicBool);
#[async_trait]
impl ContextPreparation for FailOnce {
    async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
        let request = call.request;
        if self.0.swap(false, Ordering::SeqCst) {
            return Err(anyhow!("injected preparation failure"));
        }
        Ok(PreparedContext {
            request,
            notice: None,
            usage: None,
        })
    }
}

#[tokio::test]
async fn drained_tool_reminders_survive_preparation_failure_and_flush_once_at_tail() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("failure")?.id;
    let provider = Capture::new(&["done"]);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Notices(Mutex::new(vec![
        Reminder::new("process_completion:p", "finished").with_delivery("process_completion", "p"),
    ]))))?;
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        tools,
        HarnessConfig {
            context: Arc::new(FailOnce(AtomicBool::new(true))),
            ..Default::default()
        },
    );
    let failed = harness
        .prompt(&session, "first", RunOptions::default())
        .await?;
    assert_eq!(failed.status, "failed");
    assert!(provider.requests.lock().unwrap().is_empty());
    assert!(
        !store
            .messages(&session)?
            .iter()
            .any(|m| m.origin == Some(MessageOrigin::Generated))
    );
    assert_eq!(
        harness
            .prompt(&session, "retry", RunOptions::default())
            .await?
            .status,
        "completed"
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let tail = requests[0].messages.last().unwrap();
    assert_eq!(
        tail.content,
        "<harness kind=\"process_completion:p\">finished</harness>"
    );
    assert_eq!(tail.origin, Some(MessageOrigin::Generated));
    assert_eq!(
        store
            .reminder_metadata(&session, &tail.id)?
            .unwrap()
            .reminders[0]
            .deliveries[0]
            .id,
        "p"
    );
    assert_eq!(
        store
            .messages(&session)?
            .iter()
            .filter(|m| m.id == tail.id)
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn admitted_reminder_metadata_is_atomic_and_recovered_at_the_model_tail() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("paw.db");
    let store = Store::open(&path)?;
    let session = store.create_session("accepted")?.id;
    let provider = Capture::new(&["done"]);
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    harness.start_reminders(
        &session,
        vec![Reminder::new("question_answer:q", "answer").with_delivery("question_answer", "q")],
        RunOptions::default(),
    )?;
    assert_eq!(harness.wait_for_idle(&session).await?.status, "completed");
    let request = provider.requests.lock().unwrap()[0].clone();
    let tail = request.messages.last().unwrap();
    assert_eq!(
        tail.content,
        "<harness kind=\"question_answer:q\">answer</harness>"
    );
    assert_eq!(store.queued_inputs(&session)?[0].state, "consumed");
    drop(harness);
    drop(store);
    let store = Store::open(&path)?;
    let metadata = store.reminder_metadata(&session, &tail.id)?.unwrap();
    assert_eq!(metadata.reminders[0].deliveries[0].id, "q");
    assert!(metadata.legacy_sources.is_empty());
    Ok(())
}

#[tokio::test]
async fn legacy_queued_inputs_retain_all_old_sources_through_staging_flush_and_reopen() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("paw.db");
    let store = Store::open(&path)?;
    let session = store.create_session("legacy accepted")?.id;
    let old_a = store.queue_with_origin(
        &session,
        InputKind::Steer,
        "[host notice]\nold process p completed",
        MessageOrigin::Generated,
    )?;
    let old_b = store.queue_with_origin(
        &session,
        InputKind::Steer,
        "[host notice]\nold question q answered",
        MessageOrigin::Generated,
    )?;
    let chat = store.chat_path(&session)?.unwrap();
    drop(store);
    let conn = Connection::open(&chat)?;
    conn.execute_batch("DROP TABLE reminder_metadata; DELETE FROM metadata WHERE key IN ('reminders_v1_message_cutoff','reminders_v1_input_cutoff');")?;
    drop(conn);
    let store = Store::open(&path)?;
    assert!(store.reminder_legacy_eligible(&session, &old_a.id)?);
    let provider = Capture::new(&["done"]);
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    harness.start_resume(&session, RunOptions::default())?;
    assert_eq!(harness.wait_for_idle(&session).await?.status, "completed");
    let tail = provider.requests.lock().unwrap()[0]
        .messages
        .last()
        .unwrap()
        .clone();
    assert!(tail.content.contains(&old_a.text));
    assert!(tail.content.contains(&old_b.text));
    let metadata = store.reminder_metadata(&session, &tail.id)?.unwrap();
    assert_eq!(metadata.reminders.len(), 2);
    assert_eq!(metadata.legacy_sources.len(), 2);
    assert_eq!(metadata.legacy_sources[0].record_id, old_a.id);
    assert_eq!(metadata.legacy_sources[1].text, old_b.text);
    assert!(
        store
            .queued_inputs(&session)?
            .iter()
            .all(|input| input.state == "consumed")
    );
    drop(harness);
    drop(store);
    let store = Store::open(&path)?;
    let reopened = store.reminder_metadata(&session, &tail.id)?.unwrap();
    assert_eq!(reopened, metadata);
    assert!(!store.reminder_legacy_eligible(&session, &tail.id)?);
    assert_eq!(
        store
            .reminder_metadata(&session, &old_a.id)?
            .unwrap()
            .legacy_sources[0]
            .text,
        old_a.text
    );
    Ok(())
}
