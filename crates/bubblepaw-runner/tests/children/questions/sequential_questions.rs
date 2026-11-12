use super::*;

#[tokio::test]
async fn sequential_questions_in_one_js_cell_preserve_both_accepted_answers() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session("sequential questions")?.id;
    let questions = Arc::new(QuestionTools::new(store.clone(), Arc::default(), true));
    let repl = Arc::new(CodeMode::new(
        directory.path().into(),
        Arc::new(ArtifactStore::new(
            directory.path(),
            directory.path().join("sequential-artifacts"),
        )?),
    )?);
    let provider = Arc::new(LiveAnswerProbe {
        requests: Mutex::default(),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        code: "const first = await tools.question({text:'First?'}); const second = await tools.question({text:'Second?'}); return [first.answer, second.answer]",
    });
    let mut tools = ToolRegistry::new();
    questions.register(&mut tools)?;
    repl.register(&mut tools)?;
    let root = Arc::new(Harness::new(
        store.clone(),
        provider.clone(),
        tools,
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    ));
    root.start(&session, "Ask twice", RunOptions::default())?;
    let mut ids = Vec::new();
    for answer in ["First saved", "Second saved"] {
        let question = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(question) = questions.pending(&session)? {
                    break Ok::<_, anyhow::Error>(question);
                }
                tokio::task::yield_now().await;
            }
        })
        .await??;
        let id = question["id"].as_str().unwrap().to_owned();
        assert!(!ids.contains(&id));
        if let Some(first) = ids.first() {
            assert!(
                store
                    .queued_inputs(&session)?
                    .iter()
                    .any(|input| has_answer_delivery(&store, &session, &input.id, first)),
                "first answer must be durable before replacing its question state"
            );
        }
        assert!(questions.answer(&root, &session, &id, answer)?.live);
        ids.push(id);
    }
    tokio::time::timeout(Duration::from_secs(3), provider.entered.notified()).await?;
    provider.release.notify_one();
    root.wait_for_idle(&session).await?;
    let messages = store.messages(&session)?;
    for id in ids {
        let marker = format!("<harness kind=\"question_answer:{id}\">");
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.content.contains(&marker))
                .count(),
            1
        );
    }
    assert!(messages.iter().any(|message| message.tool_call_id.is_some()
        && message.content.contains("First saved")
        && message.content.contains("Second saved")));
    root.shutdown(Duration::from_secs(2)).await?;
    repl.shutdown().await;
    Ok(())
}
