use super::*;

fn checkpoint_id(name: &str) -> String {
    format!("{CHECKPOINT_PREFIX}{name}")
}

#[test]
fn checkpoint_prune_never_scans_reference_text() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let session = store.create_session("scan")?.id;
    fn record(id: &str, patch_index: u64, boundary_seq: i64) -> crate::CheckpointRecord<'_> {
        crate::CheckpointRecord {
            id,
            patch_index,
            boundary_seq,
            metadata: "{}",
        }
    }
    store.write_checkpoint(&session, record(&checkpoint_id("a"), 1, 0), b"one", &[])?;
    let before = Store::reference_scans();
    let prunes = [(session.clone(), 3)];
    store.write_checkpoint(&session, record(&checkpoint_id("b"), 2, 5), b"two", &prunes)?;
    assert_eq!(Store::reference_scans(), before);
    Ok(())
}

#[test]
fn checkpoint_preimage_lookup_uses_the_preimage_index() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open_data_dir(directory.path())?;
    let session = store.create_session("plan")?.id;
    let id = checkpoint_id("a");
    let record = crate::CheckpointRecord {
        id: &id,
        patch_index: 1,
        boundary_seq: 1,
        metadata: "{}",
    };
    store.write_checkpoint(&session, record, b"a", &[])?;
    let conn = rusqlite::Connection::open(store.chat_path(&session)?.unwrap())?;
    // The production statement, so changing it changes what this test checks.
    let sql = format!(
        "EXPLAIN QUERY PLAN {}",
        super::deletion::CHECKPOINT_PREIMAGE_NAMED_SQL
    );
    let plan: Vec<String> = conn
        .prepare(&sql)?
        .query_map(rusqlite::params!["x"], |row| row.get::<_, String>(3))?
        .collect::<rusqlite::Result<_>>()?;
    assert!(
        plan.iter()
            .any(|line| line.contains("checkpoint_records_preimage")),
        "plan does not use the index: {plan:?}"
    );
    Ok(())
}
