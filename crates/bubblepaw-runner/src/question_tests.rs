use super::*;
use bubblepaw_core::{
    CancellationToken, HarnessConfig, ModelRequest, ModelResponse, Provider, TextCallback,
};
use std::time::Duration;

#[path = "question_parent_event_tests.rs"]
mod parent_events;

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

#[tokio::test]
async fn user_answer_errors_precede_delivery_and_do_not_change_state() -> Result<()> {
    let store = Store::in_memory()?;
    let parent = store.create_session("parent")?.id;
    let other = store.create_session("other")?.id;
    let child = store.fork(&parent, "child")?.id;
    let tools = QuestionTools::new(store.clone(), Arc::default(), true);
    let question = json!({"id":"question-id","status":"pending","text":"Choose?","run_id":"old-run","parent_session_id":parent});
    store.update_host_state(&child, "old-run", KEY, |_| Ok(question.clone()))?;
    let before = store.storage_stats()?.transaction_count;
    assert!(matches!(
        tools
            .answer_child_as_user(&other, &child, "question-id", "")
            .await,
        Err(ChildQuestionAnswerError::NotFound)
    ));
    assert!(matches!(
        tools
            .answer_child_as_user(&parent, &other, "question-id", "")
            .await,
        Err(ChildQuestionAnswerError::NotFound)
    ));
    assert!(matches!(
        tools
            .answer_child_as_user(&parent, "missing-child", "question-id", "")
            .await,
        Err(ChildQuestionAnswerError::NotFound)
    ));
    assert!(matches!(
        tools
            .answer_child_as_user(&parent, &child, "wrong-id", "")
            .await,
        Err(ChildQuestionAnswerError::NotFound)
    ));
    assert!(matches!(
        tools
            .answer_child_as_user(&parent, &child, "question-id", "  ")
            .await,
        Err(ChildQuestionAnswerError::Invalid)
    ));
    assert!(matches!(
        tools
            .answer_child_as_user(&parent, &child, "question-id", &"x".repeat(MAX_TEXT + 1))
            .await,
        Err(ChildQuestionAnswerError::Invalid)
    ));
    assert_eq!(store.host_state(&child, KEY)?, Some(question));
    assert_eq!(store.storage_stats()?.transaction_count, before);
    for status in ["answered", "cancelled"] {
        store.update_host_state(&child, "old-run", KEY, |value| {
            let mut value = value.unwrap();
            value["status"] = json!(status);
            Ok(value)
        })?;
        let before = store.storage_stats()?.transaction_count;
        assert!(matches!(
            tools
                .answer_child_as_user(&parent, &child, "question-id", "")
                .await,
            Err(ChildQuestionAnswerError::Conflict)
        ));
        assert_eq!(store.storage_stats()?.transaction_count, before);
    }
    assert_eq!(answered_by(&json!({"status":"answered"})), "parent");
    Ok(())
}

#[tokio::test]
async fn user_answer_replaces_waiting_notice_and_both_event_streams_include_attribution()
-> Result<()> {
    let store = Store::in_memory()?;
    let parent = store.create_session("parent")?.id;
    let child = store.fork(&parent, "child")?.id;
    let wake = Arc::new(WakeCoordinator::new(false, Duration::ZERO));
    let tools = Arc::new(QuestionTools::new(store.clone(), wake.clone(), true));
    let harness = Arc::new(test_harness(&store));
    tools.set_child_answer_harness(harness.clone())?;
    tools.set_child_answer_harness(harness.clone())?;
    let task_tools = tools.clone();
    let call_context = context(&child);
    let task = tokio::spawn(async move {
        task_tools
            .ask(call_context, json!({"text":"Choose?"}))
            .await
    });
    let question = pending(&tools, &child).await?;
    assert!(question["asked_at"].as_str().is_some());
    let id = question["id"].as_str().unwrap();
    tools
        .answer_child_as_user(&parent, &child, id, "Keep the original")
        .await?;
    let result: Value = serde_json::from_str(&task.await??.content)?;
    assert_eq!(result["answer"], "Keep the original");
    assert_eq!(result["answered_by"], "user");
    assert!(tools.pending(&child)?.is_none());
    let notices = wake.take_reminders(&parent, "question:");
    assert_eq!(notices.len(), 1);
    assert!(notices[0].kind.starts_with("question_answered_by_user:"));
    assert!(!notices[0].text.contains("waiting_for_answer"));
    assert!(wake.take_reminders(&parent, "question:").is_empty());
    assert!(store.host_state(&parent, KEY)?.is_none());
    for (session, key) in [(&child, "question"), (&parent, "child_question_answer")] {
        assert!(harness.events_after(0, Some(session), 100)?.iter().any(|event| {
            matches!(&event.kind, bubblepaw_core::EventKind::HostStateChanged { key: actual, value } if actual == key && value["status"] == "answered" && value["answered_by"] == "user")
        }));
    }
    assert!(matches!(
        tools
            .answer_child_as_user(&parent, &child, id, "Keep the original")
            .await,
        Err(ChildQuestionAnswerError::Conflict)
    ));
    assert!(
        tools
            .answer(&harness, &child, id, "Keep the original")
            .unwrap_err()
            .to_string()
            .contains("already answered by the user")
    );
    // Accepted parent delivery is durable and prevents restart republishing.
    store.queue_reminder(
        &parent,
        bubblepaw_core::InputKind::Steer,
        notices.into_iter().next().unwrap(),
    )?;
    assert_eq!(tools.recover_pending()?, 0);
    assert!(wake.take_reminders(&parent, "question:").is_empty());
    harness.shutdown(Duration::from_secs(2)).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_and_parent_answers_share_one_atomic_resolution() -> Result<()> {
    let store = Store::in_memory()?;
    let parent = store.create_session("parent")?.id;
    let harness = Arc::new(test_harness(&store));
    // Separate services have independent waiter locks, so only the store's
    // atomic pending check can decide the winner.
    let user_tools = Arc::new(QuestionTools::new(store.clone(), Arc::default(), true));
    let parent_tools = Arc::new(QuestionTools::new(store.clone(), Arc::default(), true));
    user_tools.set_child_answer_harness(harness.clone())?;
    for index in 0..8 {
        let child = store.fork(&parent, format!("child {index}"))?.id;
        store.update_host_state(&child, "lost-run", KEY, |_| Ok(json!({"id":"race-question","status":"pending","text":"Choose?","run_id":"lost-run","parent_session_id":parent})))?;
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let user_task = {
            let (tools, parent, child, barrier) = (
                user_tools.clone(),
                parent.clone(),
                child.clone(),
                barrier.clone(),
            );
            tokio::spawn(async move {
                barrier.wait().await;
                tools
                    .answer_child_as_user(&parent, &child, "race-question", "User answer")
                    .await
            })
        };
        let parent_task = {
            let (tools, harness, child) = (parent_tools.clone(), harness.clone(), child.clone());
            tokio::spawn(async move {
                barrier.wait().await;
                tools.answer(&harness, &child, "race-question", "Parent answer")
            })
        };
        let (user_result, parent_result) = (user_task.await?, parent_task.await?);
        assert_ne!(user_result.is_ok(), parent_result.is_ok());
        let saved = store.host_state(&child, KEY)?.unwrap();
        if user_result.is_ok() {
            assert_eq!(saved["answered_by"], "user");
            assert!(
                parent_result
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("answered")
            );
            harness.wait_for_idle(&child).await?;
        } else {
            assert!(matches!(
                user_result,
                Err(ChildQuestionAnswerError::Conflict)
            ));
            assert_eq!(saved["answered_by"], "parent");
        }
        assert_eq!(harness.events_after(0, Some(&child), 100)?.iter().filter(|event| {
            matches!(&event.kind, bubblepaw_core::EventKind::HostStateChanged { key, value } if key == KEY && value["status"] == "answered")
        }).count(), 1);
    }
    harness.shutdown(Duration::from_secs(2)).await?;
    Ok(())
}

#[test]
fn user_answer_parent_notice_previews_unicode_and_recovers_with_durable_identity() -> Result<()> {
    let store = Store::in_memory()?;
    let parent = store.create_session("parent")?.id;
    let child = store.fork(&parent, "child")?.id;
    let question = json!({"id":"saved-user-answer","status":"answered","text":"é".repeat(5000),"answer":"猫".repeat(5000),"answered_by":"user","delivery":"continuation","run_id":"lost-run","parent_session_id":parent});
    store.update_host_state(&child, "lost-run", KEY, |_| Ok(question))?;
    let wake = Arc::new(WakeCoordinator::default());
    let tools = QuestionTools::new(store.clone(), wake.clone(), true);
    assert_eq!(tools.recover_pending()?, 0);
    let notices = wake.take_reminders(&parent, "question:");
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].text.matches('猫').count(), 4000);
    assert_eq!(notices[0].text.matches('é').count(), 4000);
    assert_eq!(notices[0].deliveries[0].kind, "question_answered_by_user");
    assert_eq!(
        store.host_state(&parent, "child_question_answer")?.unwrap()["answered_by"],
        "user"
    );
    store.queue_reminder(
        &parent,
        bubblepaw_core::InputKind::Steer,
        notices.into_iter().next().unwrap(),
    )?;
    tools.recover_pending()?;
    assert!(wake.take_reminders(&parent, "question:").is_empty());
    Ok(())
}

#[tokio::test]
async fn saved_user_answer_recovers_internally_without_becoming_a_parent_answer() -> Result<()> {
    let store = Store::in_memory()?;
    let parent = store.create_session("parent")?.id;
    let child = store.fork(&parent, "child")?.id;
    store.update_host_state(&child, "lost-run", KEY, |_| Ok(json!({"id":"saved-user-answer","status":"answered","text":"Keep?","answer":"Yes","answered_by":"user","delivery":"continuation","run_id":"lost-run","parent_session_id":parent})))?;
    let tools = Arc::new(QuestionTools::new(store.clone(), Arc::default(), true));
    let harness = Arc::new(test_harness(&store));
    let answer = tools
        .unconsumed_answer(&harness, &child)?
        .expect("saved user answer remains recoverable");
    assert_eq!(answer.question["answered_by"], "user");
    assert!(!answer.live);
    continue_answer(&harness, &store, &child, &answer, None)?;
    harness.wait_for_idle(&child).await?;
    assert!(tools.unconsumed_answer(&harness, &child)?.is_none());
    assert_eq!(
        store.host_state(&child, KEY)?.unwrap()["answered_by"],
        "user"
    );
    harness.shutdown(Duration::from_secs(2)).await?;
    Ok(())
}
