//! Host user answers preserve the ordinary question delivery and child lifetime.
use super::*;
use bubblepaw_core::{EventKind, MessageOrigin};
use bubblepaw_runner::question::ChildQuestionAnswerError;

struct UserAnswerScript {
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Provider for UserAnswerScript {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        let has_result = request
            .messages
            .iter()
            .any(|message| message.tool_call_id.is_some());
        self.requests.lock().unwrap().push(request);
        if has_result {
            Ok(ModelResponse {
                text: "child continued from answer".into(),
                ..Default::default()
            })
        } else {
            Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: "user-answer-question-cell".into(),
                    name: "js".into(),
                    arguments: json!({"code":"state.kept = 'same JS cell'; const answer = await tools.question({text:'Which colour?',options:['Red','Blue']}); if (state.kept !== 'same JS cell') throw Error('lost JS cell'); return answer;"}),
                }],
                ..Default::default()
            })
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_answer_resumes_child_neutrally_and_rejects_late_parent_answer() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let parent = store.create_session("parent")?.id;
    let other_parent = store.create_session("other parent")?.id;
    let wake = Arc::new(WakeCoordinator::new(false, Duration::ZERO));
    let questions = Arc::new(QuestionTools::new(store.clone(), wake.clone(), true));
    let repl = Arc::new(CodeMode::new(
        directory.path().into(),
        Arc::new(ArtifactStore::new(
            directory.path(),
            directory.path().join("artifacts"),
        )?),
    )?);
    let script = Arc::new(UserAnswerScript {
        requests: Mutex::default(),
    });
    let mut child_tools = ToolRegistry::new();
    questions.register(&mut child_tools)?;
    repl.register(&mut child_tools)?;
    let child_harness = Arc::new(Harness::new(
        store.clone(),
        script.clone(),
        child_tools,
        HarnessConfig {
            execution_mode: bubblepaw_core::ExecutionMode::Js,
            ..Default::default()
        },
    ));
    let agents = Arc::new(
        AgentTools::from_profiles(
            store.clone(),
            vec![AgentProfile {
                name: "read-only".into(),
                description: "question fixture".into(),
                harness: child_harness.clone(),
                model: None,
                max_rounds: Some(12),
                approval: None,
            }],
        )?
        .with_registry(Arc::new(AgentRegistry::default().with_wake(wake.clone())))
        .with_repl(repl.clone()),
    );
    agents.set_questions(questions.clone())?;
    let context = ToolContext {
        session_id: parent.clone(),
        run_id: "launch".into(),
        invocation_id: "launch-child".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    let receipt = agents.start(&context, json!({"task":"Ask about colour"}))?;
    let child = receipt["sessionId"].as_str().unwrap();
    let question = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(question) = questions.pending(child)? {
                break Ok::<_, anyhow::Error>(question);
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    let id = question["id"].as_str().unwrap();
    assert!(question["asked_at"].as_str().is_some());
    assert!(matches!(
        questions
            .answer_child_as_user(&other_parent, child, id, "Blue with stripes")
            .await,
        Err(ChildQuestionAnswerError::NotFound)
    ));
    assert!(matches!(
        questions
            .answer_child_as_user(&parent, child, "wrong", "Blue with stripes")
            .await,
        Err(ChildQuestionAnswerError::NotFound)
    ));
    assert!(matches!(
        questions
            .answer_child_as_user(&parent, child, id, " ")
            .await,
        Err(ChildQuestionAnswerError::Invalid)
    ));
    let other_context = ToolContext {
        session_id: other_parent.clone(),
        ..context.clone()
    };
    assert!(
        agents
            .send(
                &other_context,
                json!({"sessionId":child,"question_id":id,"text":"Unauthorized"})
            )
            .unwrap_err()
            .to_string()
            .contains("another conversation")
    );
    questions
        .answer_child_as_user(&parent, child, id, "Blue with stripes")
        .await?;
    let late = agents
        .send(
            &context,
            json!({"sessionId":child,"question_id":id,"text":"Red"}),
        )
        .unwrap_err();
    assert!(late.to_string().contains("already answered by the user"));
    assert!(matches!(
        questions
            .answer_child_as_user(&parent, child, id, "Blue with stripes")
            .await,
        Err(ChildQuestionAnswerError::Conflict)
    ));
    child_harness.wait_for_idle(child).await?;
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        agents.wait(&parent, receipt["handle"].as_str().unwrap(), None),
    )
    .await??;
    assert_eq!(result["status"], "completed");
    assert_eq!(store.runs(child)?.len(), 1);
    assert_eq!(
        store.host_state(child, "question")?.unwrap()["answered_by"],
        "user"
    );
    let messages = store.messages(child)?;
    let neutral = messages
        .iter()
        .filter(|message| {
            message.origin == Some(MessageOrigin::Generated)
                && message.content.contains("question_answer:")
        })
        .collect::<Vec<_>>();
    assert_eq!(neutral.len(), 1);
    assert!(
        neutral[0]
            .content
            .contains("Question: Which colour?\nAnswer: Blue with stripes")
    );
    assert!(!neutral[0].content.contains("by the user"));
    assert!(!neutral[0].content.contains("Parent message"));
    assert!(messages.iter().any(|message| message.role == Role::Tool
        && message.content.contains("\"answered_by\":\"user\"")));
    assert!(
        !messages
            .iter()
            .any(|message| message.content == "Red" || message.content == "Unauthorized")
    );
    // Register the idle parent only after the user resolves the question. Its
    // waiting notice must be replaced before the required wake is admitted,
    // even though optional completion wakes are disabled in this fixture.
    let parent_script = Arc::new(NoticeProbe(Mutex::default()));
    let mut parent_tools = ToolRegistry::new();
    questions.register(&mut parent_tools)?;
    let parent_harness = Arc::new(Harness::new(
        store.clone(),
        parent_script.clone(),
        parent_tools,
        HarnessConfig::default(),
    ));
    let _parent_registration = wake.register_harness(parent_harness.clone());
    let _driver = wake.start_driver();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if store
                .runs(&parent)?
                .last()
                .is_some_and(|run| run.status == "completed")
            {
                break Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    {
        let requests = parent_script.0.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let notices = requests[0]
            .messages
            .iter()
            .filter(|message| {
                message.origin == Some(MessageOrigin::Generated)
                    && message.content.contains("question_answered_by_user:")
            })
            .collect::<Vec<_>>();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].content.contains("The user answered child"));
        assert!(notices[0].content.contains("Answer: Blue with stripes"));
        assert!(
            !requests[0]
                .messages
                .iter()
                .any(|message| message.content.contains("waiting_for_answer"))
        );
    }
    assert_eq!(store.runs(&parent)?.len(), 1);
    assert!(wake.take_reminders(&parent, "question:").is_empty());
    assert!(child_harness.events_after(0, Some(&parent), 100)?.iter().any(|event| matches!(&event.kind, EventKind::HostStateChanged { key, value } if key == "child_question_answer" && value["answered_by"] == "user")));
    // Once that answer has been consumed, ordinary idless child continuation
    // remains available; ownership checks continue to reject another parent.
    assert!(
        agents
            .send(
                &other_context,
                json!({"sessionId":child,"text":"Next task"})
            )
            .is_err()
    );
    agents.send(&context, json!({"sessionId":child,"text":"Next task"}))?;
    child_harness.wait_for_idle(child).await?;
    assert_eq!(store.runs(child)?.len(), 2);
    agents.shutdown().await?;
    repl.shutdown().await;
    child_harness.shutdown(Duration::from_secs(2)).await?;
    parent_harness.shutdown(Duration::from_secs(2)).await?;
    Ok(())
}
