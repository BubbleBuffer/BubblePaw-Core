use super::*;
use anyhow::Result;

#[tokio::test]
async fn archive_restore_and_deleted_chat_search_preserve_offline_contract() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::open_data_dir(root.path().join("data"))?;
    let session = store.create_session_with_workspace("storage roundtrip", root.path())?;
    store.append_batch(&[bubblepaw_core::Message {
        id: uuid::Uuid::new_v4().to_string(),
        session_id: session.id.clone(),
        role: bubblepaw_core::Role::User,
        origin: None,
        content: "synthetic needle".into(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: "2026-10-29T00:00:00Z".into(),
    }])?;
    let ctx = HostCtx::for_store(store.clone(), root.path());
    let search = json!({"query":"needle","session_id":session.id});
    let before = dispatch(&ctx, Profile::Read, "session.search", search.clone()).await?;
    assert_eq!(before.as_array().unwrap().len(), 1);
    let path = store.chat_path(&session.id)?.unwrap();
    let archived = path.with_extension("db.zst");
    dispatch(
        &ctx,
        Profile::Full,
        "session.archive",
        json!({"session_id":session.id}),
    )
    .await?;
    assert!(archived.exists() && !path.exists());
    dispatch(
        &ctx,
        Profile::Full,
        "session.unarchive",
        json!({"session_id":session.id}),
    )
    .await?;
    assert!(path.exists() && !archived.exists());
    assert_eq!(
        dispatch(&ctx, Profile::Read, "session.search", search.clone()).await?,
        before
    );
    dispatch(
        &ctx,
        Profile::Full,
        "session.delete",
        json!({"session_id":session.id}),
    )
    .await?;
    assert_eq!(
        dispatch(&ctx, Profile::Read, "session.search", search).await?,
        json!([])
    );
    assert_eq!(
        dispatch(
            &ctx,
            Profile::Read,
            "session.search",
            json!({"query":"needle","session_id":"unknown"})
        )
        .await?,
        json!([])
    );
    Ok(())
}
