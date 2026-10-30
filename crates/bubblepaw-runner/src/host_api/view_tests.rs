use super::*;
use anyhow::Result;

#[tokio::test]
async fn bounded_view_strips_private_provider_state_and_reuses_disposable_cache() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session_with_workspace("bounded view", root.path())?;
    let mut ctx = HostCtx::for_store(store.clone(), root.path());
    ctx.views = Some(Arc::new(std::sync::Mutex::new(views::ServingView::new()?)));
    dispatch(
        &ctx,
        Profile::Edit,
        "run.start",
        json!({"session_id":session.id,"text":"hello"}),
    )
    .await?;
    dispatch(
        &ctx,
        Profile::Read,
        "run.wait",
        json!({"session_id":session.id}),
    )
    .await?;
    let messages = (0..205)
        .map(|index| bubblepaw_core::Message {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session.id.clone(),
            role: bubblepaw_core::Role::Assistant,
            origin: None,
            content: format!("message {index}"),
            tool_calls: Vec::new(),
            tool_call_id: None,
            provider_state: Some(json!({"private":"provider-only"})),
            is_error: false,
            created_at: "2026-10-29T00:00:00Z".into(),
        })
        .collect::<Vec<_>>();
    store.append_batch(&messages)?;
    let first = dispatch(
        &ctx,
        Profile::Read,
        "session.view",
        json!({"session_id":session.id}),
    )
    .await?;
    let first: views::SessionViewResult = serde_json::from_value(first)?;
    assert_eq!(first.view.messages.len(), 200);
    assert!(first.view.limits.messages_truncated);
    assert!(
        first
            .view
            .messages
            .iter()
            .all(|message| message.provider_state.is_none())
    );
    assert!(first.view.runs.iter().all(|run| run.get("text").is_none()));
    assert_eq!(first.view.session.message_count, 207);
    let writes = ctx
        .views
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .db
        .total_changes();
    let second = dispatch(
        &ctx,
        Profile::Read,
        "session.view",
        json!({"session_id":session.id}),
    )
    .await?;
    assert_eq!(second["revision"], first.revision);
    assert_eq!(
        ctx.views
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .db
            .total_changes(),
        writes
    );
    let sidebar = dispatch(&ctx, Profile::Read, "session.sidebar", json!({})).await?;
    assert_eq!(sidebar[0]["id"], session.id);
    store.update_host_state(&session.id, "question-fixture", "question", |_| {
        Ok(json!({"id":"question","status":"pending","text":"answer?"}))
    })?;
    let view = dispatch(
        &ctx,
        Profile::Read,
        "session.view",
        json!({"session_id":session.id}),
    )
    .await?;
    assert_eq!(view["view"]["session"]["status"], "waiting_for_answer");
    assert_eq!(
        dispatch(&ctx, Profile::Read, "session.sidebar", json!({})).await?[0]["status"],
        "waiting_for_answer"
    );
    Ok(())
}

#[tokio::test]
async fn serving_cache_keeps_four_views_and_sidebar_is_bounded() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let mut ctx = HostCtx::for_store(store.clone(), root.path());
    ctx.views = Some(Arc::new(std::sync::Mutex::new(views::ServingView::new()?)));
    let mut ids = Vec::new();
    for index in 0..70 {
        ids.push(
            store
                .create_session_with_workspace(format!("session {index}"), root.path())?
                .id,
        );
    }
    assert_eq!(
        dispatch(&ctx, Profile::Read, "session.sidebar", json!({}))
            .await?
            .as_array()
            .unwrap()
            .len(),
        64
    );
    for id in ids.iter().take(6) {
        dispatch(
            &ctx,
            Profile::Read,
            "session.view",
            json!({"session_id":id}),
        )
        .await?;
    }
    let cache = ctx.views.as_ref().unwrap().lock().unwrap();
    assert_eq!(
        cache
            .db
            .query_row("SELECT COUNT(*) FROM views", [], |row| row.get::<_, i64>(0))?,
        4
    );
    Ok(())
}
