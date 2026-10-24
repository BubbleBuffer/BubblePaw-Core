//! Recovery reader contracts across structured, staged and pre-migration records.
use super::*;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, InputKind, Message, ModelRequest, ModelResponse,
    Provider, Role, RunOptions, TextCallback, ToolRegistry,
};
struct Complete;
#[async_trait]
impl Provider for Complete {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: "done".into(),
            ..Default::default()
        })
    }
}
use crate::question::{QuestionAnswer, answer_has_pending_input, answer_is_undelivered};
use serde_json::json;

fn markers() -> Vec<DeliveryMarker> {
    vec![
        DeliveryMarker::new("child_completion", r#"["child-1","run-1"]"#, "runId run-1)"),
        DeliveryMarker::new("process_completion", "process:one", "process:one"),
        DeliveryMarker::new(
            "question_answer",
            "question-1",
            "[question answer:question-1]",
        ),
    ]
}
fn message(session: &str, id: &str, text: &str) -> Message {
    Message {
        id: id.into(),
        session_id: session.into(),
        role: Role::User,
        origin: Some(MessageOrigin::Generated),
        content: text.into(),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: String::new(),
    }
}
fn answer(id: &str) -> QuestionAnswer {
    QuestionAnswer {
        question: json!({"id":id,"text":"Continue?","answer":"Yes"}),
        live: false,
    }
}
fn stage(store: &Store, session: &str, input: &str) -> Result<()> {
    // Model the exact persisted interruption point after input staging and before flush.
    let conn = rusqlite::Connection::open(store.chat_path(session)?.unwrap())?;
    assert_eq!(
        conn.execute(
            "UPDATE inputs SET state='reminder_pending' WHERE id=?1",
            [input]
        )?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn queued_staged_and_combined_reminders_recover_exact_identities_once() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("paw.db");
    let store = Store::open(&path)?;
    let session = store.create_session("structured recovery")?.id;
    for marker in markers() {
        let reminder = Reminder::new(
            format!("{}:{}", marker.kind, marker.id),
            "No legacy marker is needed",
        )
        .with_delivery(marker.kind, marker.id);
        let input = store.queue_reminder(&session, InputKind::Steer, reminder)?;
        assert!(undelivered_markers(&store, &session, markers())?.len() < 3);
        stage(&store, &session, &input.id)?;
    }
    assert!(undelivered_markers(&store, &session, markers())?.is_empty());
    assert!(answer_has_pending_input(
        &store,
        &session,
        &answer("question-1")
    )?);
    drop(store);
    let store = Store::open(&path)?;
    assert!(
        answer_has_pending_input(&store, &session, &answer("question-1"))?,
        "staged interruption still needs resume"
    );
    let harness = Arc::new(Harness::new(
        store.clone(),
        Arc::new(Complete),
        ToolRegistry::new(),
        HarnessConfig::default(),
    ));
    crate::question::continue_answer(&harness, &store, &session, &answer("question-1"), None)?;
    harness.wait_for_idle(&session).await?;
    assert_eq!(
        store.runs(&session)?.len(),
        1,
        "staged-only answer resumes one actual run"
    );
    assert_eq!(store.runs(&session)?[0].status, "completed");
    let messages = store.messages(&session)?;
    assert_eq!(
        messages.len(),
        2,
        "one combined reminder and one completed model reply"
    );
    assert!(messages[0].content.starts_with("<harness kind="));
    assert_eq!(
        store
            .reminder_metadata(&session, &messages[0].id)?
            .unwrap()
            .reminders
            .len(),
        3
    );
    assert!(undelivered_markers(&store, &session, markers())?.is_empty());
    assert!(!answer_has_pending_input(
        &store,
        &session,
        &answer("question-1")
    )?);
    assert!(!answer_is_undelivered(
        &store,
        &session,
        &answer("question-1")
    )?);
    crate::question::continue_answer(&harness, &store, &session, &answer("question-1"), None)?;
    assert_eq!(
        store.runs(&session)?.len(),
        1,
        "unchanged recovery cannot admit another run"
    );
    let before = store.messages(&session)?.len();
    drop(harness);
    drop(store);
    let store = Store::open(&path)?;
    assert!(undelivered_markers(&store, &session, markers())?.is_empty());
    assert!(!answer_has_pending_input(
        &store,
        &session,
        &answer("question-1")
    )?);
    assert_eq!(
        store.messages(&session)?.len(),
        before,
        "reopen does not duplicate structured delivery"
    );
    Ok(())
}

#[test]
fn structured_and_current_plain_records_never_match_incidental_legacy_text() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("incidental markers")?.id;
    let text = "[host notice]\nrunId run-1) process:one [question answer:question-1]";
    store.queue_reminder(
        &session,
        InputKind::Steer,
        Reminder::new("other", text).with_delivery("question_answer", "wrong-question"),
    )?;
    store.queue_with_origin(&session, InputKind::Steer, text, MessageOrigin::Generated)?;
    store.append_batch(&[message(&session, "plain-current", text)])?;
    assert_eq!(undelivered_markers(&store, &session, markers())?.len(), 3);
    assert!(!answer_has_pending_input(
        &store,
        &session,
        &answer("question-1")
    )?);
    assert!(answer_is_undelivered(
        &store,
        &session,
        &answer("question-1")
    )?);
    Ok(())
}

#[tokio::test]
async fn pre_migration_legacy_history_and_pending_answer_still_count_without_duplicates()
-> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("paw.db");
    let store = Store::open(&path)?;
    let session = store.create_session("legacy recovery")?.id;
    store.append_batch(&[message(
        &session,
        "legacy-child",
        "[host notice]\nChild (runId run-1) ended.",
    )])?;
    store.queue_with_origin(
        &session,
        InputKind::Steer,
        "[host notice]\nJob process:one exited",
        MessageOrigin::Generated,
    )?;
    let input = store.queue_with_origin(
        &session,
        InputKind::Steer,
        "[host notice]\n[question answer:question-1]\nAnswer: Yes",
        MessageOrigin::Generated,
    )?;
    let chat = store.chat_path(&session)?.unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(chat)?;
    conn.execute_batch("DROP TABLE reminder_metadata; DELETE FROM metadata WHERE key IN ('reminders_v1_message_cutoff','reminders_v1_input_cutoff');")?;
    drop(conn);
    let store = Store::open(&path)?;
    assert!(store.reminder_legacy_eligible(&session, &input.id)?);
    assert!(undelivered_markers(&store, &session, markers())?.is_empty());
    assert!(answer_has_pending_input(
        &store,
        &session,
        &answer("question-1")
    )?);
    assert!(!answer_is_undelivered(
        &store,
        &session,
        &answer("question-1")
    )?);
    let harness = Harness::new(
        store.clone(),
        Arc::new(Complete),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    harness.start_resume(&session, RunOptions::default())?;
    harness.wait_for_idle(&session).await?;
    assert!(!answer_has_pending_input(
        &store,
        &session,
        &answer("question-1")
    )?);
    let before = store.messages(&session)?.len();
    assert!(undelivered_markers(&store, &session, markers())?.is_empty());
    drop(harness);
    drop(store);
    let store = Store::open(&path)?;
    assert!(undelivered_markers(&store, &session, markers())?.is_empty());
    assert!(!answer_is_undelivered(
        &store,
        &session,
        &answer("question-1")
    )?);
    assert!(!answer_has_pending_input(
        &store,
        &session,
        &answer("question-1")
    )?);
    assert_eq!(
        store.messages(&session)?.len(),
        before,
        "reopen recovery never duplicates flushed old deliveries"
    );
    Ok(())
}
