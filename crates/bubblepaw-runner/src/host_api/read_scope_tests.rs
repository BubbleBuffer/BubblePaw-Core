use super::*;
use anyhow::Result;

async fn read(ctx: &HostCtx, method: &str, params: Value) -> Result<Value, HostError> {
    dispatch(ctx, Profile::Read, method, params).await
}

#[tokio::test]
async fn host_reads_foreign_sessions_while_workspace_reads_and_mutations_stay_separate()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let a = root.path().join("a");
    let b = root.path().join("b");
    std::fs::create_dir_all(&a)?;
    std::fs::create_dir_all(&b)?;
    let store = Store::open_data_dir(root.path().join("data"))?;
    let foreign = store.create_session_with_workspace("foreign", &b)?;
    let child =
        store.create_session_with_parent("foreign child", Default::default(), Some(&foreign.id))?;
    store.update_host_state(&child.id,"fixture","question",|_| Ok(json!({"id":"pending","status":"pending","text":"foreign question","options":[],"parent_session_id":foreign.id})))?;
    let scoped = HostCtx::for_store(store.clone(), &a);
    let params = json!({"session_id":foreign.id,"id":"missing","kind":"episode","limit":10});
    for method in [
        "session.state",
        "session.view",
        "session.history",
        "history.messages",
        "context.view",
        "context.structure",
        "context.summary",
        "context.events",
        "history.range",
        "environment.inspect",
        "process.list",
        "approval.list",
        "todo.list",
        "attachments.read",
        "artifacts.list",
        "artifacts.read",
        "events.after",
        "run.wait",
    ] {
        assert_eq!(
            read(&scoped, method, params.clone())
                .await
                .unwrap_err()
                .http_status(),
            409,
            "{method}"
        );
    }
    assert_eq!(
        read(
            &scoped,
            "question.pending_child",
            json!({"session_id":child.id})
        )
        .await
        .unwrap_err()
        .http_status(),
        409
    );
    let mut host = scoped.clone();
    host.read_scope = ReadScope::Host;
    assert_eq!(
        read(&host, "session.state", json!({"session_id":foreign.id})).await?["session"]["id"],
        foreign.id
    );
    assert_eq!(
        read(&host, "session.view", json!({"session_id":foreign.id})).await?["view"]["session"]["id"],
        foreign.id
    );
    assert_eq!(
        read(&host, "process.list", json!({"session_id":foreign.id})).await?,
        json!({"jobs":[]})
    );
    assert_eq!(
        read(
            &host,
            "question.pending_child",
            json!({"session_id":child.id})
        )
        .await?["id"],
        "pending"
    );
    assert_eq!(
        dispatch(
            &host,
            Profile::Edit,
            "session.rename",
            json!({"session_id":foreign.id,"title":"not permitted"})
        )
        .await
        .unwrap_err()
        .http_status(),
        409
    );
    // The data scope is explicit: changing only the mutation workspace does not widen it.
    let mut different_mutation_workspace = scoped.clone();
    different_mutation_workspace.workspace = b;
    assert_eq!(
        read(
            &different_mutation_workspace,
            "process.list",
            json!({"session_id":foreign.id})
        )
        .await
        .unwrap_err()
        .http_status(),
        409
    );
    Ok(())
}

#[tokio::test]
async fn workspace_lists_paginate_after_filtering_and_sidebar_cache_tracks_scope() -> Result<()> {
    let root = tempfile::tempdir()?;
    let a = root.path().join("a");
    let b = root.path().join("b");
    std::fs::create_dir_all(&a)?;
    std::fs::create_dir_all(&b)?;
    let store = Store::open_data_dir(root.path().join("data"))?;
    let first = store.create_session_with_workspace("a one", &a)?;
    let second = store.create_session_with_workspace("a two", &a)?;
    let foreign = store.create_session_with_workspace("b one", &b)?;
    let mut host = HostCtx::for_store(store, &a);
    host.read_scope = ReadScope::Host;
    host.views = Some(Arc::new(std::sync::Mutex::new(views::ServingView::new()?)));
    assert_eq!(
        read(&host, "session.sidebar", json!({}))
            .await?
            .as_array()
            .unwrap()
            .len(),
        3
    );
    read(&host, "session.view", json!({"session_id":foreign.id})).await?;
    let mut scoped = host.clone();
    scoped.read_scope = ReadScope::Workspace(a);
    let page = read(&scoped, "session.list", json!({"limit":1})).await?;
    assert_eq!(page["sessions"].as_array().unwrap().len(), 1);
    assert!(page["next_cursor"].is_string());
    let next = read(
        &scoped,
        "session.list",
        json!({"limit":1,"cursor":page["next_cursor"]}),
    )
    .await?;
    let mut ids = vec![
        page["sessions"][0]["id"].as_str().unwrap().to_owned(),
        next["sessions"][0]["id"].as_str().unwrap().to_owned(),
    ];
    ids.sort();
    let mut expected = vec![first.id, second.id];
    expected.sort();
    assert_eq!(ids, expected);
    assert!(next["next_cursor"].is_null());
    assert_eq!(
        read(&scoped, "session.sidebar", json!({}))
            .await?
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        read(&scoped, "session.view", json!({"session_id":foreign.id}))
            .await
            .unwrap_err()
            .http_status(),
        409
    );
    assert_eq!(
        read(&host, "session.sidebar", json!({}))
            .await?
            .as_array()
            .unwrap()
            .len(),
        3
    );
    Ok(())
}

#[test]
fn read_checks_do_not_bind_legacy_chats_restore_archives_or_commit_transactions() -> Result<()> {
    let root = tempfile::tempdir()?;
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace)?;
    let store = Store::open_data_dir(root.path().join("data"))?;
    let legacy = store.create_session("unbound legacy")?;
    let bound = store.create_session_with_workspace("archived", &workspace)?;
    // chat_path enters scoped(), which restores a packed archive. Capture the
    // active placement before archiving and inspect those paths directly.
    let active = store.chat_path(&bound.id)?.unwrap();
    let archive = active.with_extension("db.zst");
    assert!(store.archive_chat(&bound.id)?);
    assert!(archive.exists() && !active.exists());
    let ctx = HostCtx::for_store(store.clone(), workspace);
    let before = store.storage_stats()?;
    ctx.ensure_read_session(&legacy.id)?;
    ctx.ensure_read_session(&bound.id)?;
    assert!(ctx.read_scope.allows(&store, &legacy.id)?);
    assert!(ctx.read_scope.allows(&store, &bound.id)?);
    assert!(store.session_workspace(&legacy.id)?.is_none());
    assert!(store.bound_workspace()?.is_none());
    assert!(archive.exists() && !active.exists());
    let after = store.storage_stats()?;
    assert_eq!(after.transaction_count, before.transaction_count);
    assert_eq!(
        after.catalog_transaction_count,
        before.catalog_transaction_count
    );
    Ok(())
}
