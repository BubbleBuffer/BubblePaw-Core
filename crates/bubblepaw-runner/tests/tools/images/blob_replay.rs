use super::*;

#[tokio::test]
async fn canonical_image_blob_is_stored_once_and_replayed_after_file_removal() {
    let fixture = Fixture::new(true);
    // Incompressible bytes exercise the SQLite blob cost, rather than a tiny
    // highly-compressible fixture whose duplication would be hard to measure.
    let mut bytes = PNG.to_vec();
    let mut state = 0x61295a71u32;
    while bytes.len() < 256_000 {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    std::fs::write(fixture.root.path().join("image.png"), &bytes).unwrap();
    let initialized = fixture
        .harness
        .prompt(&fixture.session, "initialize store", RunOptions::default())
        .await
        .unwrap();
    assert!(
        initialized.error.is_none(),
        "store warm-up failed: {:?}",
        initialized.error
    );
    fixture.store.idle_checkpoint().unwrap();
    let baseline = fixture.store.storage_stats().unwrap();
    let reference = store_image(&fixture.store, &fixture.session, "image/png", &bytes).unwrap();
    let before = fixture.store.storage_stats().unwrap();
    for _ in 0..20 {
        assert_eq!(
            store_image(&fixture.store, &fixture.session, "image/png", &bytes).unwrap(),
            reference
        );
    }
    assert_eq!(
        fixture.store.storage_stats().unwrap().transaction_count,
        before.transaction_count
    );
    for _ in 0..8 {
        fixture
            .cell("await tools.read({path:'image.png'}); return 'inspected';")
            .await;
    }
    std::fs::remove_file(fixture.root.path().join("image.png")).unwrap();
    fixture.manager.shutdown().await;
    fixture.store.idle_checkpoint().unwrap();
    let conn =
        rusqlite::Connection::open(fixture.store.chat_path(&fixture.session).unwrap().unwrap())
            .unwrap();
    let (count, raw, compressed): (i64, i64, i64) = conn.query_row("SELECT count(*),sum(raw_bytes),sum(length(data)) FROM blobs WHERE hash=(SELECT blob_hash FROM attachments WHERE id=?1)", [&reference.id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
    assert_eq!(count, 1);
    assert_eq!(raw, i64::try_from(bytes.len()).unwrap());
    assert!(compressed < raw + 1024);
    let data: Vec<String> = conn
        .prepare("SELECT data FROM messages")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(data.iter().all(
        |data| !data.contains(&STANDARD.encode(&bytes)) && !data.contains("BubblePawImageWire")
    ));
    let stats = fixture.store.storage_stats().unwrap();
    let growth = stats.database_bytes.saturating_sub(baseline.database_bytes);
    assert!(
        growth < bytes.len() as u64 + 128 * 1024,
        "unexpected repeated image storage growth: {growth}"
    );
    eprintln!(
        "image storage: original={raw} blob_compressed={compressed} db_baseline={} db={} growth={growth} wal={} transactions={} duplicate_attachment_writes=0",
        baseline.database_bytes, stats.database_bytes, stats.wal_bytes, stats.transaction_count
    );
    let Fixture {
        root,
        store,
        harness,
        manager,
        session,
        requests: old_requests,
    } = fixture;
    drop(conn);
    drop(old_requests);
    drop(harness);
    drop(manager);
    drop(store);
    let reopened = Store::open(root.path().join("record.db")).unwrap();
    let recorded = reopened.messages(&session).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = ImageProvider::new(
        Arc::new(Cells {
            requests: requests.clone(),
            vision: true,
        }),
        reopened,
    );
    provider
        .generate(
            ModelRequest {
                model: "fixture".into(),
                session_id: session,
                messages: recorded,
                ..Default::default()
            },
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .messages
            .iter()
            .any(|message| message.role == Role::Tool
                && message.content.contains(&STANDARD.encode(&bytes)))
    );
}
