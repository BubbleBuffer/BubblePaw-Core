use super::*;
use anyhow::Result;

async fn search(ctx: &HostCtx, params: Value) -> Result<Value> {
    Ok(dispatch(ctx, Profile::Read, "session.list", params).await?)
}

#[tokio::test]
async fn titles_are_literal_unicode_lowercase_substrings_and_query_selects_pages() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let wanted = store.create_session("Résumé 100%_ [draft]")?;
    store.create_session("Different title")?;
    let ctx = HostCtx::for_store(store.clone(), root.path());
    for query in ["RÉSUMÉ", "100%_", "[draft]", " 100"] {
        let page = search(&ctx, json!({"query":query})).await?;
        assert_eq!(page["sessions"].as_array().unwrap().len(), 1, "{query}");
        assert_eq!(page["sessions"][0]["id"], wanted.id);
        assert!(page["next_cursor"].is_null());
    }
    let none = search(&ctx, json!({"query":"absent"})).await?;
    assert_eq!(none, json!({"sessions":[],"next_cursor":null}));
    assert!(search(&ctx, json!({})).await?.is_array());
    assert!(search(&ctx, json!({"query":null})).await?.is_array());
    assert_eq!(
        search(&ctx, json!({"query":""})).await?["sessions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    store.rename_session(&wanted.id, "Renamed title")?;
    assert_eq!(
        search(&ctx, json!({"query":"résumé"})).await?["sessions"],
        json!([])
    );
    assert_eq!(
        search(&ctx, json!({"query":"renamed"})).await?["sessions"][0]["id"],
        wanted.id
    );
    Ok(())
}

#[tokio::test]
async fn title_search_spans_projects_keeps_archives_cold_and_respects_visibility_and_scope()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let a = root.path().join("a");
    let b = root.path().join("b");
    std::fs::create_dir_all(&a)?;
    std::fs::create_dir_all(&b)?;
    let store = Store::open_data_dir(root.path().join("data"))?;
    let first = store.create_session_with_workspace("Shared project A", &a)?;
    let archived = store.create_session_with_workspace("Shared project B", &b)?;
    store.create_session_with_visibility(
        "Shared internal",
        Default::default(),
        Some(&first.id),
        true,
    )?;
    store.create_session_with_workspace("Unrelated", &a)?;
    let archived_path = store.chat_path(&archived.id)?.unwrap();
    assert!(store.archive_chat(&archived.id)?);
    assert!(!archived_path.exists());
    let mut ctx = HostCtx::for_store(store.clone(), &a);
    ctx.read_scope = ReadScope::Host;
    let page = search(&ctx, json!({"query":"SHARED","limit":10})).await?;
    let rows = page["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row["id"] == first.id));
    let row = rows.iter().find(|row| row["id"] == archived.id).unwrap();
    assert_eq!(row["workspace"], json!(b.canonicalize()?));
    assert!(row["usage"].is_null());
    assert_eq!(row["needs_you"], false);
    assert_eq!(row["running"], false);
    assert!(
        !archived_path.exists(),
        "search must not restore an archive"
    );
    ctx.read_scope = ReadScope::Workspace(a);
    let page = search(&ctx, json!({"query":"shared","limit":1})).await?;
    assert_eq!(page["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(page["sessions"][0]["id"], first.id);
    assert!(page["next_cursor"].is_null());
    assert!(!archived_path.exists());
    Ok(())
}

#[tokio::test]
async fn title_search_filters_before_creation_order_paging() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let mut expected = Vec::new();
    for title in [
        "match one",
        "other",
        "MATCH two",
        "different",
        "match three",
    ] {
        let session = store.create_session(title)?;
        if title.to_lowercase().contains("match") {
            expected.push(session);
        }
    }
    expected.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    let ctx = HostCtx::for_store(store, root.path());
    let mut cursor = None;
    let mut found = Vec::new();
    loop {
        let page = search(&ctx, json!({"query":"match","limit":1,"cursor":cursor})).await?;
        let rows = page["sessions"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        found.push(rows[0]["id"].as_str().unwrap().to_owned());
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
        assert!(found.len() < 3, "last page must not have a cursor");
    }
    assert_eq!(
        found,
        expected
            .into_iter()
            .map(|session| session.id)
            .collect::<Vec<_>>()
    );
    for params in [
        json!({"query":true}),
        json!({"query":"match","limit":0}),
        json!({"query":"match","limit":1001}),
        json!({"query":"match","cursor":"bad"}),
    ] {
        assert_eq!(
            dispatch(&ctx, Profile::Read, "session.list", params)
                .await
                .unwrap_err()
                .rpc_code(),
            -32000
        );
    }
    Ok(())
}

/// Synthetic scale check: 10,000 catalog entries, with two real root records.
/// The other 9,998 deliberately have no chat files. Success therefore also
/// verifies that searching unmatched titles never opens their root records.
#[tokio::test]
#[ignore = "explicit 10k catalog title-search measurement"]
async fn title_search_10k_catalog_measurement() -> Result<()> {
    use std::time::Instant;
    let root = tempfile::tempdir()?;
    let data = root.path().join("data");
    let store = Store::open_data_dir(&data)?;
    let active = store.create_session_with_workspace("Needle active", root.path())?;
    let archived = store.create_session_with_workspace("Needle archived", root.path())?;
    let archived_path = store.chat_path(&archived.id)?.unwrap();
    assert!(store.archive_chat(&archived.id)?);
    let mut catalog = rusqlite::Connection::open(data.join("catalog.db"))?;
    let tx = catalog.transaction()?;
    for index in 0..9_998 {
        let mut session = active.clone();
        session.id = format!("synthetic-{index:05}");
        session.title = format!("Other catalog title {index:05}");
        tx.execute(
            "INSERT INTO sessions(id,parent_id,root_id,title,last_activity,created_at,file,storage_state,data) VALUES(?1,NULL,?1,?2,?3,?3,?4,'active',?5)",
            rusqlite::params![session.id, session.title, session.created_at, format!("chats/{}.db",session.id), serde_json::to_string(&session)?],
        )?;
    }
    tx.commit()?;
    assert_eq!(
        catalog.query_row("SELECT count(*) FROM sessions", [], |row| row
            .get::<_, i64>(0))?,
        10_000
    );
    let plan: String = catalog.query_row(
        "EXPLAIN QUERY PLAN SELECT data,last_activity FROM sessions",
        [],
        |row| row.get(3),
    )?;
    let mut ctx = HostCtx::for_store(store, root.path());
    ctx.read_scope = ReadScope::Host;
    let lookups_before = ctx.store.catalog_lookups();
    let first_start = Instant::now();
    let first = search(&ctx, json!({"query":"needle","limit":1})).await?;
    let first_ms = first_start.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(first["sessions"].as_array().unwrap().len(), 1);
    let second = search(
        &ctx,
        json!({"query":"needle","limit":1,"cursor":first["next_cursor"]}),
    )
    .await?;
    assert_eq!(second["sessions"].as_array().unwrap().len(), 1);
    assert!(second["next_cursor"].is_null());
    assert_ne!(first["sessions"][0]["id"], second["sessions"][0]["id"]);
    let mut hit_ms = Vec::new();
    let mut miss_ms = Vec::new();
    for _ in 0..20 {
        let start = Instant::now();
        search(&ctx, json!({"query":"needle","limit":1})).await?;
        hit_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        let start = Instant::now();
        let none = search(&ctx, json!({"query":"absent","limit":100})).await?;
        miss_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(none["sessions"], json!([]));
    }
    hit_ms.sort_by(f64::total_cmp);
    miss_ms.sort_by(f64::total_cmp);
    println!(
        "title_search_10k: catalog_rows=10000 real_root_records=2 missing_unmatched_root_records=9998 plan={plan:?} first_hit_ms={first_ms:.3} warm_repeats=20 hit_p50_ms={:.3} hit_p95_ms={:.3} miss_p50_ms={:.3} miss_p95_ms={:.3} scoped_catalog_lookups={}",
        hit_ms[9],
        hit_ms[18],
        miss_ms[9],
        miss_ms[18],
        ctx.store.catalog_lookups() - lookups_before
    );
    assert!(!archived_path.exists());
    Ok(())
}
