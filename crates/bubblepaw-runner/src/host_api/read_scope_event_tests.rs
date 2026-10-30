use super::*;
use anyhow::Result;

#[tokio::test]
async fn events_and_search_apply_read_scope_without_changing_run_ownership() -> Result<()> {
    let root = tempfile::tempdir()?;
    let a = root.path().join("a");
    let b = root.path().join("b");
    std::fs::create_dir_all(&a)?;
    std::fs::create_dir_all(&b)?;
    let store = Store::open_data_dir(root.path().join("data"))?;
    let first = store.create_session_with_workspace("a", &a)?;
    let foreign = store.create_session_with_workspace("b", &b)?;
    let scoped = HostCtx::for_store(store, &a);
    let mut other = scoped.clone();
    other.workspace = b.clone();
    other.read_scope = ReadScope::Workspace(b);
    let mut live = events::subscribe(&scoped, None, None)?.receiver;
    for (ctx, id) in [(&scoped, &first.id), (&other, &foreign.id)] {
        dispatch(
            ctx,
            Profile::Edit,
            "run.start",
            json!({"session_id":id,"text":"synthetic needle"}),
        )
        .await?;
        dispatch(ctx, Profile::Read, "run.wait", json!({"session_id":id})).await?;
    }
    let mut host = scoped.clone();
    host.read_scope = ReadScope::Host;
    let all = dispatch(&host, Profile::Read, "events.after", json!({"after":0})).await?;
    assert!(
        all["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["session_id"] == foreign.id)
    );
    let local = dispatch(&scoped, Profile::Read, "events.after", json!({"after":0})).await?;
    assert!(!local["events"].as_array().unwrap().is_empty());
    assert!(
        local["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["session_id"] == first.id)
    );
    let replay = events::subscribe(&scoped, Some(0), None)?;
    assert!(
        replay
            .replay
            .iter()
            .all(|event| event.session_id == first.id)
    );
    assert_eq!(
        events::subscribe(&scoped, Some(0), Some(&foreign.id))
            .err()
            .unwrap()
            .http_status(),
        409
    );
    let foreign_event = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let event = live.recv().await?;
            if event.session_id == foreign.id {
                return Ok::<_, anyhow::Error>(event);
            }
        }
    })
    .await??;
    assert!(!events::visible(&scoped, &foreign_event, None));
    assert!(events::visible(&host, &foreign_event, None));
    let query = json!({"query":"needle"});
    let all = dispatch(&host, Profile::Read, "session.search", query.clone()).await?;
    assert_eq!(all.as_array().unwrap().len(), 2);
    let local = dispatch(&scoped, Profile::Read, "session.search", query).await?;
    assert_eq!(local.as_array().unwrap().len(), 1);
    assert_eq!(local[0]["session_id"], first.id);
    assert_eq!(
        dispatch(
            &scoped,
            Profile::Read,
            "session.search",
            json!({"query":"needle","session_id":foreign.id})
        )
        .await
        .unwrap_err()
        .http_status(),
        409
    );
    Ok(())
}
