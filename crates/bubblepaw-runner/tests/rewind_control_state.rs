//! Scripted real-tool acceptance of historical todo and pending-question forks.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, Role,
    RunOptions, Store, TextCallback, ToolContext, ToolRegistry,
};
use bubblepaw_runner::{
    artifacts::ArtifactStore, codemode::CodeMode, question::QuestionTools, rewind::rewind_session,
    todo, tools::WorkspaceTools,
};
use serde_json::{Value, json};
use std::{fs, sync::Arc, time::Duration};

struct NoModel;
#[async_trait]
impl Provider for NoModel {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        panic!("script acceptance must not call a provider")
    }
}

async fn script(harness: &Harness, session: &str, code: String) -> Result<()> {
    let run = harness
        .prompt(
            session,
            "scripted control fixture",
            RunOptions {
                script: Some(code),
                ..Default::default()
            },
        )
        .await?;
    assert!(run.error.is_none(), "{:?}", run.error);
    Ok(())
}

fn todo_value(store: &Store, session: &str) -> Result<Value> {
    Ok(serde_json::to_value(todo::list(store, session)?)?)
}

#[tokio::test]
async fn rewind_uses_target_todo_and_omits_an_ended_question_waiter_after_reopen() -> Result<()> {
    let root = tempfile::tempdir()?;
    let data = tempfile::tempdir()?;
    fs::write(root.path().join("a"), "original\n")?;
    let a = json!([{"text":"A","status":"pending"}]);
    let b = json!([{"text":"B","status":"done"}]);
    let (source, fork, question, source_messages) = {
        let store = Store::open_data_dir(data.path())?;
        bubblepaw_runner::rewind::install_checkpoint_sweep(&store)?;
        let source = store.create_session("rewind control source")?.id;
        let questions = Arc::new(QuestionTools::new(store.clone(), Arc::default(), true));
        let mut tools = ToolRegistry::new();
        todo::register(&mut tools, store.clone())?;
        questions.register(&mut tools)?;
        WorkspaceTools::new(root.path())?.register(&mut tools, false)?;
        let question_tool = tools.get("question").unwrap();
        let repl = Arc::new(CodeMode::new(
            root.path().into(),
            Arc::new(ArtifactStore::new(
                root.path(),
                data.path().join("artifacts"),
            )?),
        )?);
        repl.register(&mut tools)?;
        let harness = Harness::new(
            store.clone(),
            Arc::new(NoModel),
            tools,
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                ..Default::default()
            },
        );

        // Nested tool return values are intentionally discarded. Canonical JS
        // output cannot serve as the authoritative state history.
        script(&harness, &source, format!(
            "await tools.todo({{items:[{{text:'before A',status:'pending'}}]}}); await tools.todo({{items:{a}}}); return 'saved';"
        )).await?;
        assert_eq!(todo_value(&store, &source)?, a);
        let edit = json!({"path":"a","old_string":"original","new_string":"changed"});
        script(&harness, &source, format!(
            "await tools.todo({{items:[{{text:'before B',status:'in_progress'}}]}}); await tools.todo({{items:{b}}}); await tools.edit({edit}); return 'changed';"
        )).await?;
        let messages = store.messages(&source)?;
        let source_messages = serde_json::to_value(&messages)?;
        let changed_user = messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .unwrap()
            .id
            .clone();
        let changed_result = messages.last().unwrap().id.clone();
        assert_eq!(fs::read_to_string(root.path().join("a"))?, "changed\n");

        // Exercise the registered question tool, then lose only its volatile
        // waiter. This creates a genuine durable pending question without
        // inventing question state or replaying any interrupted external effect.
        let context = ToolContext {
            session_id: source.clone(),
            run_id: "ended-question-waiter".into(),
            invocation_id: "ended-question-waiter".into(),
            cancellation: CancellationToken::new(),
            approval: Default::default(),
        };
        let waiter = tokio::spawn(async move {
            question_tool
                .invoke(context, json!({"text":"Keep which?"}))
                .await
        });
        let pending = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(question) = questions.pending(&source)? {
                    return Ok::<_, anyhow::Error>(question);
                }
                tokio::task::yield_now().await;
            }
        })
        .await??;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert_eq!(questions.pending(&source)?, Some(pending.clone()));

        // Only the latest user message is a rewind target. The tool result after
        // the changed cell is rejected; the whole cell reverts to its user message.
        let error = rewind_session(&store, root.path(), &source, &changed_result).unwrap_err();
        assert!(format!("{error:#}").contains("only the last message can be rewound"));
        assert_eq!(fs::read_to_string(root.path().join("a"))?, "changed\n");
        let before_cell = rewind_session(&store, root.path(), &source, &changed_user)?;
        assert!(before_cell.conflicts.is_empty());
        assert_eq!(todo_value(&store, &before_cell.session_id)?, a);
        assert_eq!(before_cell.restored, ["a"]);
        assert_eq!(fs::read_to_string(root.path().join("a"))?, "original\n");

        // A fork's own latest user message is its rewind target. Its todo state
        // and the source's pending question remain canonical to each session.
        let fork_latest = store
            .messages(&before_cell.session_id)?
            .into_iter()
            .rfind(bubblepaw_core::is_typed_user)
            .unwrap()
            .id;
        let repeated = rewind_session(&store, root.path(), &before_cell.session_id, &fork_latest)?;
        assert!(repeated.conflicts.is_empty());
        assert_eq!(todo_value(&store, &repeated.session_id)?, a);
        assert!(questions.pending(&repeated.session_id)?.is_none());
        assert_eq!(todo_value(&store, &source)?, b);
        assert_eq!(questions.pending(&source)?, Some(pending.clone()));

        // Fork updates use their own state and attachments. Clearing one fork
        // cannot change either the source or the separately retained A fork.
        script(
            &harness,
            &before_cell.session_id,
            "await tools.todo({items:[]}); return 'cleared';".into(),
        )
        .await?;
        assert_eq!(todo_value(&store, &source)?, b);
        assert_eq!(todo_value(&store, &repeated.session_id)?, a);
        assert_eq!(questions.pending(&source)?, Some(pending.clone()));
        assert_eq!(
            serde_json::to_value(store.messages(&source)?)?,
            source_messages
        );
        repl.shutdown().await;
        harness.shutdown(Duration::from_secs(2)).await?;
        (source, repeated.session_id, pending, source_messages)
    };
    let store = Store::open_data_dir(data.path())?;
    let questions = QuestionTools::new(store.clone(), Arc::default(), true);
    assert_eq!(todo_value(&store, &source)?, b);
    assert_eq!(questions.pending(&source)?, Some(question));
    assert_eq!(
        serde_json::to_value(store.messages(&source)?)?,
        source_messages
    );
    assert_eq!(todo_value(&store, &fork)?, a);
    assert!(questions.pending(&fork)?.is_none());
    assert_eq!(store.host_state(&fork, "question")?, None);
    assert_eq!(fs::read_to_string(root.path().join("a"))?, "original\n");
    Ok(())
}
