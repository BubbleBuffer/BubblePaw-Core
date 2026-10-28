use super::*;
use crate::{EventKind, HarnessEvent};

struct NeverCalled;
#[async_trait::async_trait]
impl crate::Provider for NeverCalled {
    async fn generate(
        &self,
        _: crate::ModelRequest,
        _: crate::TextCallback,
        _: crate::CancellationToken,
    ) -> Result<crate::ModelResponse> {
        bail!("recovery tests must not call a model")
    }
}

fn harness(store: &Store) -> crate::Harness {
    crate::Harness::new(
        store.clone(),
        Arc::new(NeverCalled),
        crate::ToolRegistry::new(),
        crate::HarnessConfig::default(),
    )
}

fn running(session: &str) -> Run {
    Run {
        id: id(),
        session_id: session.into(),
        model: "scripted-recovery".into(),
        status: "running".into(),
        text: None,
        error: None,
        started_at: now(),
        ended_at: None,
        usage: None,
    }
}

fn events(store: &Store) -> Result<Vec<HarnessEvent>> {
    harness(store).events_after(0, None, 1000)
}

fn assert_interrupted_parent_event(event: &HarnessEvent, parent: &str, run: &Run) {
    assert!(event.durable);
    assert_eq!(event.session_id, parent);
    assert_eq!(event.run_id, run.id);
    assert!(matches!(
        &event.kind,
        EventKind::ChildRunStateChanged { child_session_id, state }
            if child_session_id == &run.session_id && state == "interrupted"
    ));
}

#[test]
fn reopen_publishes_one_durable_parent_interrupted_event() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (parent, run, old_sequence) = {
        let store = Store::open_data_dir(directory.path())?;
        let parent = store.create_session("parent")?;
        let child = store.create_session_with_parent(
            "child",
            ContextStrategy::default(),
            Some(&parent.id),
        )?;
        let run = running(&child.id);
        store.start_run(&run)?;
        (parent.id, run, store.runtime().log.last())
    };
    let first_sequence = {
        let store = Store::open_data_dir(directory.path())?;
        assert_eq!(store.run(&run.id)?.status, "interrupted");
        let events = events(&store)?;
        assert_eq!(events.len(), 1);
        assert_interrupted_parent_event(&events[0], &parent, &run);
        assert!(events[0].sequence > old_sequence);
        assert_eq!(
            store.storage_stats()?.chat_transaction_count,
            2,
            "one existing recovery write plus one parent event transaction"
        );
        store.flush_serving()?;
        events[0].sequence
    };
    let reopened = Store::open_data_dir(directory.path())?;
    let events = events(&reopened)?;
    assert_eq!(
        events.len(),
        1,
        "reopening interrupted runs does not re-emit"
    );
    assert_eq!(events[0].sequence, first_sequence);
    assert_interrupted_parent_event(&events[0], &parent, &run);
    Ok(())
}

#[test]
fn root_and_hidden_recovery_add_no_event_transaction() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let runs = {
        let store = Store::open_data_dir(directory.path())?;
        let parent = store.create_session("parent")?;
        let hidden = store.create_session_with_visibility(
            "hidden",
            ContextStrategy::default(),
            Some(&parent.id),
            true,
        )?;
        let runs = [running(&parent.id), running(&hidden.id)];
        for run in &runs {
            store.start_run(run)?;
        }
        runs
    };
    let store = Store::open_data_dir(directory.path())?;
    for run in &runs {
        assert_eq!(store.run(&run.id)?.status, "interrupted");
    }
    assert!(events(&store)?.is_empty());
    assert_eq!(
        store.storage_stats()?.chat_transaction_count,
        2,
        "only the two existing run recovery writes"
    );
    Ok(())
}

#[test]
fn recovery_after_layout_startup_publishes_without_waiting_for_reopen() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let parent = store.create_session("parent")?;
    let child =
        store.create_session_with_parent("child", ContextStrategy::default(), Some(&parent.id))?;
    let run = running(&child.id);
    store.start_run(&run)?;
    let before = store.storage_stats()?.chat_transaction_count;
    store.scoped(&child.id)?.unwrap().recover()?;
    let events = events(&store)?;
    assert_eq!(events.len(), 1);
    assert_interrupted_parent_event(&events[0], &parent.id, &run);
    assert_eq!(store.storage_stats()?.chat_transaction_count, before + 2);
    Ok(())
}

#[test]
fn legacy_single_file_recovery_commits_state_and_parent_event_together() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("legacy.sqlite");
    let (parent, run) = {
        let store = Store::initialize(Connection::open(&path)?, Some(path.clone()))?;
        let parent = store.create_session("parent")?;
        let child = store.create_session_with_parent(
            "child",
            ContextStrategy::default(),
            Some(&parent.id),
        )?;
        let run = running(&child.id);
        store.start_run(&run)?;
        store.write(|tx| {
            tx.execute(
                "UPDATE runs SET result_message_id='retained-result' WHERE id=?1",
                [&run.id],
            )?;
            Ok(())
        })?;
        (parent.id, run)
    };
    let store = Store::initialize(Connection::open(&path)?, Some(path.clone()))?;
    assert_eq!(store.run(&run.id)?.status, "interrupted");
    assert_eq!(
        store.run_row(&run.id)?.1.as_deref(),
        Some("retained-result")
    );
    let events = events(&store)?;
    assert_eq!(events.len(), 1);
    assert_interrupted_parent_event(&events[0], &parent, &run);
    assert_eq!(store.storage_stats()?.chat_transaction_count, 1);
    Ok(())
}

#[test]
fn directory_restart_invalidates_cursors_if_queued_recovery_was_lost() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (run, old_sequence) = {
        let store = Store::open_data_dir(directory.path())?;
        let parent = store.create_session("parent")?;
        let child = store.create_session_with_parent(
            "child",
            ContextStrategy::default(),
            Some(&parent.id),
        )?;
        let run = running(&child.id);
        store.start_run(&run)?;
        // Reproduce the interruption window after canonical recovery but
        // before startup drains the disposable parent event queue.
        store
            .0
            .layout
            .as_ref()
            .unwrap()
            .recovery
            .ready
            .store(false, Ordering::Release);
        store.scoped(&child.id)?.unwrap().recover()?;
        assert!(events(&store)?.is_empty());
        (run, store.runtime().log.last())
    };
    let store = Store::open_data_dir(directory.path())?;
    assert_eq!(store.run(&run.id)?.status, "interrupted");
    let harness = harness(&store);
    assert!(harness.last_sequence() > old_sequence);
    assert!(harness.pruned_through() > old_sequence);
    assert!(
        !harness.can_resume_from(old_sequence),
        "clients must resnapshot the canonical interrupted run"
    );
    Ok(())
}
