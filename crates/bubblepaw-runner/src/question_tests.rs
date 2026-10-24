use super::*;
use bubblepaw_core::{
    CancellationToken, HarnessConfig, ModelRequest, ModelResponse, Provider, TextCallback,
};
use std::time::Duration;

struct Idle;
#[async_trait]
impl Provider for Idle {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse::default())
    }
}
/// Answers queue through this harness, so it must share the test store.
fn test_harness(store: &Store) -> Harness {
    Harness::new(
        store.clone(),
        Arc::new(Idle),
        ToolRegistry::new(),
        HarnessConfig::default(),
    )
}

fn context(session: &str) -> ToolContext {
    ToolContext {
        session_id: session.into(),
        run_id: "question-test".into(),
        invocation_id: "question-call".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    }
}
async fn pending(tools: &QuestionTools, session: &str) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(value) = tools.pending(session)? {
                return Ok(value);
            }
            tokio::task::yield_now().await;
        }
    })
    .await?
}
#[tokio::test]
async fn root_question_accepts_free_text_and_cancellation() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("question")?.id;
    let tools = Arc::new(QuestionTools::new(store.clone(), Arc::default(), true));
    let task_tools = tools.clone();
    let call_context = context(&session);
    let task = tokio::spawn(async move {
        task_tools
            .ask(call_context, json!({"text":"Choose","options":["A","B"]}))
            .await
    });
    let question = pending(&tools, &session).await?;
    let harness = test_harness(&store);
    assert!(tools.answer(&harness, &session, "wrong-id", "C").is_err());
    assert!(
        tools
            .ask(context(&session), json!({"text":"duplicate"}))
            .await
            .is_err()
    );
    let answer = tools.answer(
        &harness,
        &session,
        question["id"].as_str().unwrap(),
        "C, with details",
    )?;
    assert!(answer.live);
    let result: Value = serde_json::from_str(&task.await??.content)?;
    assert_eq!(result["answer"], "C, with details");
    assert!(tools.pending(&session)?.is_none());
    // This direct-tool fixture has no run metadata. Use another session
    // for cancellation rather than pretending its answer was model-consumed.
    let cancelled_session = store.create_session("cancel question")?.id;
    let task_tools = tools.clone();
    let call_context = context(&cancelled_session);
    let token = call_context.cancellation.clone();
    let task =
        tokio::spawn(async move { task_tools.ask(call_context, json!({"text":"Again?"})).await });
    pending(&tools, &cancelled_session).await?;
    token.cancel();
    let result: Value = serde_json::from_str(&task.await??.content)?;
    assert_eq!(result["status"], "cancelled");
    assert!(tools.pending(&cancelled_session)?.is_none());
    Ok(())
}
#[tokio::test]
async fn unattended_root_returns_immediately_but_child_routes_to_direct_parent() -> Result<()> {
    let store = Store::in_memory()?;
    let root = store.create_session("root")?.id;
    let child = store.fork(&root, "child")?.id;
    let grandchild = store.fork(&child, "grandchild")?.id;
    let wake = Arc::new(WakeCoordinator::default());
    let tools = Arc::new(QuestionTools::new(store.clone(), wake.clone(), false));
    let before = store.storage_stats()?.transaction_count;
    let result: Value = serde_json::from_str(
        &tools
            .ask(context(&root), json!({"text":"Unavailable?"}))
            .await?
            .content,
    )?;
    assert_eq!(result["status"], "no_user_available");
    assert_eq!(store.storage_stats()?.transaction_count, before);
    let task_tools = tools.clone();
    let call_context = context(&grandchild);
    let task = tokio::spawn(async move {
        task_tools
            .ask(call_context, json!({"text":"Parent?"}))
            .await
    });
    pending(&tools, &grandchild).await?;
    assert!(wake.take_reminders(&root, "question:").is_empty());
    let notices = wake.take_reminders(&child, "question:");
    assert_eq!(notices.len(), 1);
    assert!(notices[0].text.contains("waiting_for_answer"));
    assert!(tools.cancel(&grandchild)?);
    assert!(task.await??.content.contains("cancelled"));
    Ok(())
}
#[tokio::test]
async fn pending_record_survives_loss_of_waiter_and_reopen() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("questions.db");
    let store = Store::open(&path)?;
    let parent = store.create_session("parent")?.id;
    let session = store.fork(&parent, "child")?.id;
    let tools = Arc::new(QuestionTools::new(store.clone(), Arc::default(), true));
    let before = store.storage_stats()?;
    let task_tools = tools.clone();
    let call_context = context(&session);
    let task = tokio::spawn(async move {
        task_tools
            .ask(call_context, json!({"text":"Durable?"}))
            .await
    });
    let question = pending(&tools, &session).await?;
    let after = store.storage_stats()?;
    assert_eq!(after.transaction_count - before.transaction_count, 1);
    assert!(after.wal_bytes.saturating_sub(before.wal_bytes) < 128 * 1024);
    task.abort();
    let _ = task.await;
    drop(tools);
    drop(store);
    let store = Store::open(&path)?;
    let wake = Arc::new(WakeCoordinator::default());
    let tools = QuestionTools::new(store.clone(), wake.clone(), true);
    assert_eq!(tools.pending(&session)?, Some(question.clone()));
    assert_eq!(tools.recover_pending()?, 1);
    assert_eq!(wake.take_reminders(&parent, "question:").len(), 1);
    let harness = test_harness(&store);
    let answer = tools.answer(&harness, &session, question["id"].as_str().unwrap(), "Yes")?;
    assert!(!answer.live);
    assert!(answer.continuation_text().contains("[question answer:"));
    assert_eq!(store.host_state(&session, KEY)?.unwrap()["answer"], "Yes");
    assert!(store.pending_effects(&session)?.is_empty());
    assert!(
        !tools
            .answer(&harness, &session, question["id"].as_str().unwrap(), "Yes")?
            .live
    );
    assert!(
        tools
            .answer(
                &harness,
                &session,
                question["id"].as_str().unwrap(),
                "Different"
            )
            .is_err()
    );
    Ok(())
}
#[tokio::test]
async fn new_question_preserves_an_answer_awaiting_continuation_admission() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("saved answer")?.id;
    let value = json!({"id":"unadmitted","status":"answered","text":"Original?","answer":"Keep this","delivery":"continuation","run_id":"lost-run"});
    store.update_host_state(&session, "lost-run", KEY, |_| Ok(value.clone()))?;
    let tools = QuestionTools::new(store.clone(), Arc::default(), true);
    let error = tools
        .ask(context(&session), json!({"text":"Overwrite?"}))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("still awaits continuation admission")
    );
    assert_eq!(store.host_state(&session, KEY)?, Some(value));
    Ok(())
}
