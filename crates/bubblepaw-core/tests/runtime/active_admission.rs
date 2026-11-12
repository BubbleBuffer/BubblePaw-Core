use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, ModelRequest, ModelResponse, Provider, RunOptions,
    Store, TextCallback, ToolRegistry,
};
use std::sync::Arc;

struct Answer;
#[async_trait]
impl Provider for Answer {
    async fn generate(
        &self,
        _: ModelRequest,
        on_text: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        on_text("complete");
        Ok(ModelResponse {
            text: "complete".into(),
            ..Default::default()
        })
    }
}

fn harness(store: &Store) -> Harness {
    Harness::new(
        store.clone(),
        Arc::new(Answer),
        ToolRegistry::new(),
        HarnessConfig::default(),
    )
}

#[test]
fn attachment_metadata_pages_are_bounded_literal_and_session_scoped() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for store in [Store::in_memory()?, Store::open_data_dir(directory.path())?] {
        let session = store.create_session("metadata")?;
        let other = store.create_session("other")?;
        for (owner, id, metadata) in [
            (&session.id, "notice_%:a", "first"),
            (&session.id, "notice_%:b", "second"),
            (&session.id, "notice_xy:c", "not the literal prefix"),
            (&other.id, "notice_%:a", "another owner"),
        ] {
            store.store_attachment(owner, id, metadata, b"blob not requested")?;
        }
        assert_eq!(
            store.attachment_metadata_page(&session.id, "notice_%:", None, 1)?,
            vec![("notice_%:a".into(), "first".into())]
        );
        assert_eq!(
            store.attachment_metadata_page(&session.id, "notice_%:", Some("notice_%:a"), 1)?,
            vec![("notice_%:b".into(), "second".into())]
        );
        assert!(
            store
                .attachment_metadata_page(&session.id, "notice_%:", Some("notice_%:b"), 1)?
                .is_empty()
        );
        assert!(
            store
                .attachment_metadata_page(&session.id, "notice_%:", None, 0)?
                .is_empty()
        );
    }
    Ok(())
}

#[tokio::test]
async fn active_only_admission_works_with_memory_and_directory_stores() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for store in [Store::in_memory()?, Store::open_data_dir(directory.path())?] {
        let session = store.create_session("active")?;
        assert_eq!(
            store.active_session_metadata(&session.id)?.unwrap().id,
            session.id
        );
        assert!(store.active_session_metadata("missing")?.is_none());
        let agent = harness(&store);
        agent.start_if_active(&session.id, "start", RunOptions::default())?;
        assert_eq!(agent.wait_for_idle(&session.id).await?.status, "completed");
        store.delete_session(&session.id)?;
        assert!(store.active_session_metadata(&session.id)?.is_none());
        assert!(
            agent
                .start_if_active(&session.id, "deleted", RunOptions::default())
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn active_only_admission_does_not_restore_archived_root_or_child() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let root = store.create_session("root")?;
    let child = store.create_session_with_parent("child", root.context_strategy, Some(&root.id))?;
    let chat = store.chat_path(&root.id)?.unwrap();
    assert!(store.archive_chat(&root.id)?);
    assert!(!chat.exists());
    let agent = harness(&store);
    for session in [&root.id, &child.id] {
        assert!(store.active_session_metadata(session)?.is_none());
        assert!(
            agent
                .start_if_active(session, "background input", RunOptions::default())
                .is_err()
        );
        assert!(!chat.exists(), "availability checks restored an archive");
    }
    // Ordinary explicit admission remains able to restore the chat.
    agent.start(&root.id, "explicit input", RunOptions::default())?;
    assert_eq!(agent.wait_for_idle(&root.id).await?.status, "completed");
    assert!(chat.exists());
    Ok(())
}
