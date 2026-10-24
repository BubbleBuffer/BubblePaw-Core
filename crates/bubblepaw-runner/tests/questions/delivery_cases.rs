//! Question reminder delivery checks; legacy compatibility lives in reminder_recovery_tests.
use super::*;

pub(super) fn has_answer_delivery(
    store: &Store,
    session: &str,
    record: &str,
    question: &str,
) -> bool {
    store
        .reminder_metadata(session, record)
        .unwrap()
        .is_some_and(|metadata| {
            metadata
                .reminders
                .iter()
                .flat_map(|reminder| &reminder.deliveries)
                .any(|delivery| delivery.kind == "question_answer" && delivery.id == question)
        })
}

#[tokio::test]
async fn answer_with_lost_waiter_resumes_already_queued_input_once() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("queued answer")?.id;
    let question = json!({"id":"already-queued","status":"answered","text":"Continue?","answer":"Yes","delivery":"continuation","run_id":"lost-run"});
    store.update_host_state(&session, "lost-run", "question", |_| Ok(question.clone()))?;
    store.queue_reminder(
        &session,
        bubblepaw_core::InputKind::Steer,
        bubblepaw_core::Reminder::new(
            "question_answer:already-queued",
            "Question: Continue?\nAnswer: Yes",
        )
        .with_delivery("question_answer", "already-queued"),
    )?;
    let provider = Arc::new(NoticeProbe(Mutex::default()));
    let root = Arc::new(Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig::default(),
    ));
    let answer = bubblepaw_runner::question::QuestionAnswer {
        question,
        live: false,
    };
    bubblepaw_runner::question::continue_answer(&root, &store, &session, &answer, None)?;
    root.wait_for_idle(&session).await?;
    bubblepaw_runner::question::continue_answer(&root, &store, &session, &answer, None)?;
    assert_eq!(store.runs(&session)?.len(), 1);
    assert_eq!(provider.0.lock().unwrap().len(), 1);
    assert_eq!(
        store
            .messages(&session)?
            .iter()
            .filter(|message| has_answer_delivery(&store, &session, &message.id, "already-queued"))
            .count(),
        1
    );
    root.shutdown(Duration::from_secs(2)).await?;
    Ok(())
}

#[tokio::test]
async fn saved_answer_recovers_on_next_root_request_without_repeated_injection() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let data = directory.path().join("saved-answer");
    let session = {
        let store = Store::open_data_dir(&data)?;
        let session = store.create_session("answer before admission")?.id;
        store.update_host_state(&session, "lost-run", "question", |_| Ok(json!({"id":"unadmitted-answer","status":"answered","text":"Keep which?","answer":"Keep the original","delivery":"continuation","run_id":"lost-run","parent_session_id":null})))?;
        session
    };
    let store = Store::open_data_dir(&data)?;
    let questions = Arc::new(QuestionTools::new(store.clone(), Arc::default(), true));
    let provider = Arc::new(NoticeProbe(Mutex::default()));
    let mut tools = ToolRegistry::new();
    questions.register(&mut tools)?;
    let root = Arc::new(Harness::new(
        store.clone(),
        provider.clone(),
        tools,
        HarnessConfig::default(),
    ));
    assert!(
        store.runs(&session)?.is_empty(),
        "restart must not start a model turn"
    );
    root.start(&session, "Continue normally", RunOptions::default())?;
    root.wait_for_idle(&session).await?;
    root.start(&session, "Next ordinary turn", RunOptions::default())?;
    root.wait_for_idle(&session).await?;
    {
        let requests = provider.0.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            let notices = request
                .messages
                .iter()
                .filter(|message| {
                    message
                        .content
                        .contains("<harness kind=\"question_answer:unadmitted-answer\">")
                })
                .collect::<Vec<_>>();
            assert_eq!(notices.len(), 1);
            assert!(notices[0].content.starts_with("<harness "));
            assert!(notices[0].content.contains("Answer: Keep the original"));
        }
    }
    assert_eq!(
        store
            .messages(&session)?
            .iter()
            .filter(|message| message
                .content
                .contains("<harness kind=\"question_answer:unadmitted-answer\">"))
            .count(),
        1
    );
    root.shutdown(Duration::from_secs(2)).await?;
    Ok(())
}
