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

/// Explicit IO-scale check using 10,000 actual root SQLite databases created
/// through the normal store API. Run with a process file-descriptor limit of
/// at least 65,536: the host caches active root connections during startup.
#[tokio::test]
#[ignore = "explicit 10k real-root title-search measurement; requires 65536 file descriptors"]
async fn title_search_10k_catalog_measurement() -> Result<()> {
    use std::time::Instant;
    let root = tempfile::tempdir()?;
    let data = root.path().join("data");
    let create_start = Instant::now();
    let store = Store::open_data_dir(&data)?;
    let first_match = store.create_session_with_workspace("Needle one", root.path())?;
    let second_match = store.create_session_with_workspace("Needle two", root.path())?;
    for index in 2..10_000 {
        store.create_session_with_workspace(format!("Other chat title {index:05}"), root.path())?;
        if (index + 1) % 1_000 == 0 {
            println!(
                "title_search_10k_setup: real_roots_created={} elapsed_seconds={:.3}",
                index + 1,
                create_start.elapsed().as_secs_f64()
            );
        }
    }
    let create_seconds = create_start.elapsed().as_secs_f64();
    // Closing and reopening is fixture setup, excluded from every query timing.
    // Opening the host scans active roots and may warm their filesystem pages.
    let close_start = Instant::now();
    drop(store);
    let close_seconds = close_start.elapsed().as_secs_f64();
    let open_start = Instant::now();
    let store = Store::open_data_dir(&data)?;
    let host_open_seconds = open_start.elapsed().as_secs_f64();
    let catalog = rusqlite::Connection::open(data.join("catalog.db"))?;
    assert_eq!(
        catalog.query_row("SELECT count(*) FROM sessions WHERE parent_id IS NULL AND root_id=id AND storage_state='active'", [], |row| row
            .get::<_, i64>(0))?,
        10_000
    );
    let root_files = catalog
        .prepare("SELECT file FROM sessions WHERE root_id=id")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    assert_eq!(root_files.len(), 10_000);
    let mut root_sqlite_bytes = 0;
    for file in root_files {
        let path = data.join(file);
        assert!(
            path.is_file(),
            "missing real root database {}",
            path.display()
        );
        root_sqlite_bytes += std::fs::metadata(path)?.len();
    }
    let physical_root_sqlite_files = std::fs::read_dir(data.join("chats"))?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "db")
        })
        .count();
    assert_eq!(physical_root_sqlite_files, 10_000);
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
    let first_miss_start = Instant::now();
    let first_miss = search(&ctx, json!({"query":"absent","limit":100})).await?;
    let first_miss_ms = first_miss_start.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(first_miss["sessions"], json!([]));
    let second = search(
        &ctx,
        json!({"query":"needle","limit":1,"cursor":first["next_cursor"]}),
    )
    .await?;
    assert_eq!(second["sessions"].as_array().unwrap().len(), 1);
    assert!(second["next_cursor"].is_null());
    assert_ne!(first["sessions"][0]["id"], second["sessions"][0]["id"]);
    for session in [&first_match, &second_match] {
        assert!(
            first["sessions"][0]["id"] == session.id || second["sessions"][0]["id"] == session.id
        );
    }
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
        "title_search_10k: catalog_rows=10000 real_active_root_sqlite_files={physical_root_sqlite_files} root_sqlite_bytes={root_sqlite_bytes} setup_create_seconds={create_seconds:.3} setup_close_seconds={close_seconds:.3} host_open_seconds={host_open_seconds:.3} plan={plan:?} first_hit_ms={first_ms:.3} first_miss_ms={first_miss_ms:.3} warm_repeats=20 hit_p50_ms={:.3} hit_p95_ms={:.3} miss_p50_ms={:.3} miss_p95_ms={:.3} scoped_catalog_lookups={}",
        hit_ms[9],
        hit_ms[18],
        miss_ms[9],
        miss_ms[18],
        ctx.store.catalog_lookups() - lookups_before
    );
    Ok(())
}
