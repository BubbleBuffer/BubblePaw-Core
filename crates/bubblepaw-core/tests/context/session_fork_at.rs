use anyhow::{Result, bail};
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextPreparation, ContextStrategy, Harness, HarnessConfig, Message,
    ModelRequest, ModelResponse, PrepareCall, PrepareMode, PreparedContext, Provider, Role,
    RunOptions, Store, TextCallback, ToolCall, ToolRegistry,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;

fn message(session: &str, id: &str, role: Role) -> Message {
    Message {
        id: id.into(),
        session_id: session.into(),
        role,
        origin: None,
        content: format!("content of {id}"),
        tool_calls: Vec::new(),
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: "2026-10-15T00:00:00Z".into(),
    }
}

#[test]
fn fork_at_copies_inclusive_history_and_preserves_the_source() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for store in [Store::in_memory()?, Store::open_data_dir(directory.path())?] {
        let source = store.create_session_with_strategy("source", ContextStrategy::Classical)?;
        let other =
            store.create_session_with_parent("other", ContextStrategy::Lcm, Some(&source.id))?;
        let first = message(&source.id, "first", Role::User);
        let mut cutoff = message(&source.id, "cutoff", Role::Assistant);
        cutoff.provider_state = Some(json!({"signature":"preserved"}));
        let last = message(&source.id, "last", Role::User);
        // A different session's interleaved row must not affect parent_cutoff.
        store.append_batch(&[
            first.clone(),
            message(&other.id, "foreign", Role::User),
            cutoff.clone(),
            last.clone(),
        ])?;
        let original = store.messages(&source.id)?;
        let metadata = serde_json::to_value(store.session(&source.id)?)?;
        let fork = store.fork_at(&source.id, &cutoff.id, "branch")?;
        assert_eq!(fork.title, "branch");
        assert_eq!(fork.parent_session_id.as_deref(), Some(source.id.as_str()));
        assert_eq!(fork.parent_cutoff, Some(2));
        assert_eq!(fork.context_strategy, ContextStrategy::Classical);
        let copied = store.messages(&fork.id)?;
        assert_eq!(copied.len(), 2);
        for (actual, expected) in copied.iter().zip(&original) {
            assert_ne!(actual.id, expected.id);
            assert_eq!(actual.session_id, fork.id);
            let mut expected = expected.clone();
            expected.id = actual.id.clone();
            expected.session_id = fork.id.clone();
            assert_eq!(*actual, expected);
        }
        assert_eq!(store.messages(&source.id)?, original);
        assert_eq!(serde_json::to_value(store.session(&source.id)?)?, metadata);
        assert!(store.context_view(&fork.id)?.is_none());
        assert!(store.context_checkpoint(&fork.id)?.is_none());
        let full = store.fork(&source.id, "full")?;
        assert_eq!(full.parent_cutoff, Some(3));
        assert_eq!(store.messages(&full.id)?.len(), 3);
        let first_only = store.fork_at(&source.id, &first.id, "first only")?;
        assert_eq!(first_only.parent_cutoff, Some(1));
        assert_eq!(store.messages(&first_only.id)?.len(), 1);
        let through_last = store.fork_at(&source.id, &last.id, "last included")?;
        assert_eq!(store.messages(&through_last.id)?.len(), 3);
        if store.chat_path(&source.id)?.is_some() {
            store.idle_checkpoint()?;
            let fork_id = fork.id;
            drop(store);
            let reopened = Store::open_data_dir(directory.path())?;
            assert_eq!(reopened.messages(&fork_id)?, copied);
            assert_eq!(reopened.session(&fork_id)?.parent_cutoff, Some(2));
            assert_eq!(reopened.messages(&source.id)?, original);
        }
    }
    Ok(())
}

#[test]
fn fork_at_rejects_missing_foreign_and_incomplete_tool_boundaries_without_creating_sessions()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    for store in [Store::in_memory()?, Store::open_data_dir(directory.path())?] {
        let source = store.create_session("source")?;
        let foreign = store.create_session("foreign")?;
        let user = message(&source.id, "user", Role::User);
        let mut calls = message(&source.id, "calls", Role::Assistant);
        calls.tool_calls = ["one", "two"]
            .into_iter()
            .map(|id| ToolCall {
                id: id.into(),
                name: "synthetic".into(),
                arguments: json!({}),
            })
            .collect();
        let mut one = message(&source.id, "result-one", Role::Tool);
        one.tool_call_id = Some("one".into());
        let mut two = message(&source.id, "result-two", Role::Tool);
        two.tool_call_id = Some("two".into());
        two.is_error = true;
        store.append_batch(&[
            user.clone(),
            calls,
            one,
            two,
            message(&foreign.id, "foreign-message", Role::User),
        ])?;
        let original = store.messages(&source.id)?;
        let count = store.list_sessions()?.len();
        for cutoff in ["missing", "foreign-message", "calls", "result-one"] {
            assert!(store.fork_at(&source.id, cutoff, "invalid").is_err());
            assert_eq!(store.list_sessions()?.len(), count);
            assert_eq!(store.messages(&source.id)?, original);
        }
        assert!(store.fork_at("missing", &user.id, "invalid").is_err());
        let resolved = store.fork_at(&source.id, "result-two", "resolved")?;
        assert_eq!(resolved.parent_cutoff, Some(4));
        assert_eq!(store.messages(&resolved.id)?.len(), 4);
        // A pending call later in history does not invalidate an earlier prefix.
        let pending = store.create_session("pending")?;
        let mut call = message(&pending.id, "pending-call", Role::Assistant);
        call.tool_calls = original[1].tool_calls.clone();
        store.append_batch(&[message(&pending.id, "before-call", Role::User), call])?;
        assert!(store.fork(&pending.id, "invalid full fork").is_err());
        assert!(
            store
                .fork_at(&pending.id, "before-call", "valid prefix")
                .is_ok()
        );
        let empty = store.create_session("empty")?;
        assert!(store.fork_at(&empty.id, "missing", "invalid").is_err());
        assert_eq!(store.fork(&empty.id, "empty fork")?.parent_cutoff, Some(0));
    }
    Ok(())
}

#[derive(Default)]
struct Blocking {
    compacting: Notify,
}

#[async_trait]
impl Provider for Blocking {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        cancellation.cancelled().await;
        bail!("cancelled")
    }
}

#[async_trait]
impl ContextPreparation for Blocking {
    async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
        if call.mode == PrepareMode::Request {
            return Ok(PreparedContext {
                request: call.request,
                notice: None,
                usage: None,
            });
        }
        self.compacting.notify_one();
        call.cancellation.cancelled().await;
        bail!("cancelled")
    }
}

#[tokio::test]
async fn fork_at_retains_active_run_and_compaction_guards() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for store in [Store::in_memory()?, Store::open_data_dir(directory.path())?] {
        let blocking = Arc::new(Blocking::default());
        let harness = Harness::new(
            store.clone(),
            blocking.clone(),
            ToolRegistry::new(),
            HarnessConfig {
                context: blocking.clone(),
                ..Default::default()
            },
        );
        let source = store.create_session("source")?;
        store.append_batch(&[message(&source.id, "cutoff", Role::User)])?;
        harness.start(&source.id, "block", RunOptions::default())?;
        assert_eq!(
            store
                .fork_at(&source.id, "cutoff", "busy")
                .unwrap_err()
                .to_string(),
            "Cannot fork an active session"
        );
        harness.cancel(&source.id);
        tokio::time::timeout(Duration::from_secs(5), harness.wait_for_idle(&source.id)).await??;
        let compacting = {
            let harness = harness.clone();
            let session_id = source.id.clone();
            tokio::spawn(async move { harness.compact_session(&session_id, None).await })
        };
        tokio::time::timeout(Duration::from_secs(5), blocking.compacting.notified()).await?;
        assert_eq!(
            store
                .fork_at(&source.id, "cutoff", "busy")
                .unwrap_err()
                .to_string(),
            "Cannot fork a session during compaction"
        );
        harness.cancel(&source.id);
        assert!(
            tokio::time::timeout(Duration::from_secs(5), compacting)
                .await??
                .is_err()
        );
        assert!(store.fork_at(&source.id, "cutoff", "idle").is_ok());
    }
    Ok(())
}

#[test]
fn fork_at_rejects_dispatched_effects_even_beyond_the_cutoff() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let source = store.create_session("source")?;
    store.append_batch(&[message(&source.id, "cutoff", Role::User)])?;
    let conn = rusqlite::Connection::open(store.chat_path(&source.id)?.unwrap())?;
    conn.execute(
        "INSERT INTO effects(id,session_id,run_id,call_id,state,detail) VALUES('effect',?1,'run','call','dispatched','{}')",
        [&source.id],
    )?;
    let count = store.list_sessions()?.len();
    assert_eq!(
        store
            .fork_at(&source.id, "cutoff", "unsafe")
            .unwrap_err()
            .to_string(),
        "Cannot fork with active background effects"
    );
    assert_eq!(store.list_sessions()?.len(), count);
    Ok(())
}

#[test]
fn fork_at_callback_errors_leave_the_committed_fork_readable() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for store in [Store::in_memory()?, Store::open_data_dir(directory.path())?] {
        let source = store.create_session("source")?;
        store.append_batch(&[message(&source.id, "cutoff", Role::User)])?;
        let mut committed = None;
        let error = store
            .fork_at_with(&source.id, "cutoff", "committed", |fork| {
                assert_eq!(store.messages(&fork.id)?.len(), 1);
                assert_eq!(store.messages(&source.id)?.len(), 1);
                committed = Some(fork.id.clone());
                bail!("host work failed")
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "host work failed");
        let fork = store.session(&committed.unwrap())?;
        assert_eq!(fork.title, "committed");
        assert_eq!(fork.parent_cutoff, Some(1));
        assert_eq!(store.messages(&fork.id)?.len(), 1);
        // The failed callback releases admission normally.
        assert!(store.fork_at(&source.id, "cutoff", "after error").is_ok());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_at_callback_holds_admission_until_host_work_finishes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for store in [Store::in_memory()?, Store::open_data_dir(directory.path())?] {
        let source = store.create_session("source")?;
        store.append_batch(&[message(&source.id, "cutoff", Role::User)])?;
        let harness = Harness::new(
            store.clone(),
            Arc::new(Blocking::default()),
            ToolRegistry::new(),
            HarnessConfig::default(),
        );
        let (entered_send, entered_receive) = tokio::sync::oneshot::channel();
        let (release_send, release_receive) = std::sync::mpsc::channel();
        let forking = {
            let store = store.clone();
            let source_id = source.id.clone();
            tokio::task::spawn_blocking(move || {
                store.fork_at_with(&source_id, "cutoff", "branch", |fork| {
                    assert_eq!(store.messages(&fork.id)?.len(), 1);
                    entered_send.send(()).unwrap();
                    release_receive.recv_timeout(Duration::from_secs(5))?;
                    Ok(())
                })
            })
        };
        tokio::time::timeout(Duration::from_secs(5), entered_receive).await??;
        let (start_ready_send, start_ready_receive) = tokio::sync::oneshot::channel();
        let mut starting = {
            let harness = harness.clone();
            let source_id = source.id.clone();
            tokio::task::spawn_blocking(move || {
                start_ready_send.send(()).unwrap();
                harness.start(&source_id, "block", RunOptions::default())
            })
        };
        let (delete_ready_send, delete_ready_receive) = tokio::sync::oneshot::channel();
        let mut deleting = {
            let store = store.clone();
            let source_id = source.id.clone();
            tokio::task::spawn_blocking(move || {
                delete_ready_send.send(()).unwrap();
                store.delete_session(&source_id)
            })
        };
        tokio::time::timeout(Duration::from_secs(5), start_ready_receive).await??;
        tokio::time::timeout(Duration::from_secs(5), delete_ready_receive).await??;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut starting)
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut deleting)
                .await
                .is_err()
        );
        release_send.send(())?;
        let _fork = tokio::time::timeout(Duration::from_secs(5), forking).await???;
        let started = tokio::time::timeout(Duration::from_secs(5), starting).await??;
        let _deleted = tokio::time::timeout(Duration::from_secs(5), deleting).await??;
        if started.is_ok() {
            harness.cancel(&source.id);
            tokio::time::timeout(Duration::from_secs(5), harness.wait_for_idle(&source.id))
                .await??;
        }
    }
    Ok(())
}
