#![cfg(feature = "host")]
use anyhow::Result;
use bubblepaw_sdk::providers::{ProviderRegistry, ScriptedProvider};
use bubblepaw_sdk::store::Store;
use bubblepaw_sdk::{Client, HostBuilder};
use std::sync::Arc;

#[tokio::test]
async fn typed_title_search_uses_host_paging_and_reports_invalid_limits() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    store.create_session("SDK match one")?;
    store.create_session("SDK MATCH two")?;
    store.create_session("Different")?;
    let client = Client::embedded(
        HostBuilder::new(
            store,
            ProviderRegistry::new("scripted", Arc::new(ScriptedProvider::new("unused"))),
        )
        .workspace(root.path()),
    )
    .await?;
    let first = client.search_session_titles("sdk match", 1, None).await?;
    assert_eq!(first.sessions.len(), 1);
    assert!(first.sessions[0].usage.is_object());
    assert!(!first.sessions[0].needs_you);
    let second = client
        .search_session_titles("sdk match", 1, first.next_cursor)
        .await?;
    assert_eq!(second.sessions.len(), 1);
    assert_ne!(first.sessions[0].session.id, second.sessions[0].session.id);
    assert!(second.next_cursor.is_none());
    assert!(
        client
            .search_session_titles("missing", 10, None)
            .await?
            .sessions
            .is_empty()
    );
    assert!(matches!(
        client.search_session_titles("sdk", 0, None).await,
        Err(bubblepaw_sdk::ClientError::Rpc { code: -32000, .. })
    ));
    client.close().await?;
    Ok(())
}
