//! SDK callers use the same descriptors and admission path as the transports.
use anyhow::Result;
use bubblepaw_sdk::{
    HostBuilder, Profile, dispatch,
    host_api::{METHODS, SessionParams},
    providers::{ProviderRegistry, ScriptedProvider},
    store::Store,
};
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn sdk_dispatches_the_shared_table() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let host = HostBuilder::new(
        Store::in_memory()?,
        ProviderRegistry::new("scripted", Arc::new(ScriptedProvider::new("sdk answer"))),
    )
    .workspace(directory.path())
    .todo(true)
    .build()?;
    assert!(METHODS.iter().any(|method| method.name == "run.start"));
    let denied = dispatch(host.api(), Profile::Read, "session.open", json!({})).await;
    assert_eq!(denied.unwrap_err().rpc_code(), -32003);
    assert!(host.store.list_sessions()?.is_empty());

    let session = dispatch(host.api(), Profile::Full, "session.open", json!({})).await?;
    let id = session["id"].as_str().unwrap();
    dispatch(
        host.api(),
        Profile::Full,
        "run.start",
        json!({"session_id":id,"text":"hello"}),
    )
    .await?;
    let params = serde_json::to_value(SessionParams {
        session_id: id.into(),
    })?;
    let run = dispatch(host.api(), Profile::Full, "run.wait", params).await?;
    assert_eq!(run["status"], "completed");
    assert_eq!(run["text"], "sdk answer");
    Ok(())
}
