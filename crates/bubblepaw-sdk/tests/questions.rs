//! The child-question operation through stable SDK paths only. Scripted, local.
use anyhow::Result;
use bubblepaw_sdk::{
    HostBuilder,
    harness::{HarnessConfig, RunOptions},
    providers::{ProviderRegistry, ScriptedProvider, ScriptedStep},
    questions::{ChildQuestionAnswerError, QuestionTools, WakeCoordinator},
    store::{Role, Store},
    tools::ToolCall,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn pending_child_question_reads_legacy_state_without_local_question_service() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let store = Store::open_data_dir(workspace.path())?;
    let parent = store.create_session("legacy parent")?;
    let child =
        store.create_session_with_parent("legacy child", Default::default(), Some(&parent.id))?;
    store.update_host_state(&child.id, "", "question", |_| {
        Ok(json!({
            "id":"legacy-question", "status":"pending", "text":"Saved question?",
            "options":null, "parent_session_id":parent.id,
        }))
    })?;
    let host = HostBuilder::new(
        store,
        ProviderRegistry::new("scripted", Arc::new(ScriptedProvider::new("unused"))),
    )
    .workspace(workspace.path())
    .todo(true)
    .build()?;
    let pending = host.pending_child_question(&child.id)?.unwrap();
    assert_eq!(pending.id, "legacy-question");
    assert_eq!(pending.text, "Saved question?");
    assert!(pending.options.is_empty());
    assert!(pending.asked_at.is_none());
    assert_eq!(pending.parent_session_id, parent.id);
    assert!(host.pending_child_question("missing")?.is_none());
    assert!(host.pending_child_question(&parent.id)?.is_none());
    assert!(matches!(
        host.answer_child_question(&parent.id, &child.id, &pending.id, "yes")
            .await,
        Err(ChildQuestionAnswerError::Internal(_))
    ));
    Ok(())
}

#[tokio::test]
async fn host_user_answers_a_live_child_question_through_stable_sdk_paths() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let parent = store.create_session("SDK parent")?;
    let child =
        store.create_session_with_parent("SDK child", Default::default(), Some(&parent.id))?;
    let questions = Arc::new(QuestionTools::new(
        store.clone(),
        Arc::new(WakeCoordinator::default()),
        true,
    ));
    let provider = ScriptedProvider::with_steps(
        vec![ScriptedStep::Calls {
            calls: vec![ToolCall {
                id: "sdk-question".into(),
                name: "question".into(),
                arguments: json!({"text":"Which colour?","options":["Blue","Green"]}),
            }],
        }],
        "Child continued.",
    );
    let host = HostBuilder::new(
        store.clone(),
        ProviderRegistry::new("scripted", Arc::new(provider)),
    )
    .workspace(workspace.path())
    .questions(questions)
    .config(HarnessConfig {
        model: "scripted".into(),
        ..Default::default()
    })
    .build()?;
    assert!(
        host.tools
            .capabilities()
            .iter()
            .all(|tool| tool.name != "answer_child_question")
    );
    host.harness
        .start(&child.id, "Ask a question", RunOptions::default())?;
    let pending = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(question) = host.pending_child_question(&child.id)? {
                return Ok::<_, anyhow::Error>(question);
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    assert_eq!(pending.text, "Which colour?");
    assert_eq!(pending.options, ["Blue", "Green"]);
    assert_eq!(pending.parent_session_id, parent.id);
    assert!(pending.asked_at.is_some());
    assert!(matches!(
        host.answer_child_question(&parent.id, &child.id, "stale", "Green")
            .await,
        Err(ChildQuestionAnswerError::NotFound)
    ));
    assert!(host.pending_child_question(&child.id)?.is_some());
    host.answer_child_question(&parent.id, &child.id, &pending.id, "Green with stripes")
        .await?;
    let run = tokio::time::timeout(
        Duration::from_secs(5),
        host.harness.wait_for_idle(&child.id),
    )
    .await??;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert!(host.pending_child_question(&child.id)?.is_none());
    assert!(store.messages(&child.id)?.iter().any(|message| {
        message.role == Role::Tool && message.content.contains("Green with stripes")
    }));
    assert_eq!(store.runs(&child.id)?.len(), 1);
    assert!(store.runs(&parent.id)?.is_empty());
    assert!(store.host_state(&parent.id, "question")?.is_none());
    assert_eq!(
        store
            .host_state(&parent.id, "child_question_answer")?
            .unwrap()["child_session_id"],
        child.id
    );
    Ok(())
}
