use super::*;

/// Every context table is emptied for a deleted session and keeps the rows of
/// the surviving one.
#[test]
fn deleting_a_session_empties_every_context_table() -> Result<()> {
    let store = Store::in_memory()?;
    let doomed = store.create_session("doomed")?;
    let kept = store.create_session("kept")?;
    let mut seqs = Vec::new();
    for session in [&doomed, &kept] {
        store.append_batch(&[Store::message(&session.id, Role::User, "hello".into())])?;
        seqs.push(store.read(|conn| {
            Ok(conn.query_row(
                "SELECT MAX(seq) FROM messages WHERE session_id=?1",
                [&session.id],
                |row| row.get::<_, i64>(0),
            )?)
        })?);
    }
    // One row per table, as `?1` session, `?2` row id, `?3` message sequence.
    let rows = [
        ("context_checkpoints", "?1,?2,?3,'{}'"),
        ("context_checkpoint_versions", "?2,?1,'hash'"),
        ("context_chapters", "?1,?2,1,?3,?3,'{}'"),
        ("context_subject_chapters", "?2,?1,1,1,1,'open','{}'"),
        ("context_episodes", "?2,?1,1,?3,?3,?2,'{}'"),
        ("context_decisions", "?2,?1,1,'split','{}'"),
        ("context_events", "?2,?1,'applied','{}'"),
        ("context_views", "?1,'{}'"),
    ];
    store.write(|tx| {
        tx.execute("INSERT INTO blobs VALUES('hash',1,x'00')", [])?;
        for (session, seq) in [&doomed, &kept].into_iter().zip(&seqs) {
            // Row ids repeat across tables so that `?2` also links rows.
            for (table, values) in rows {
                let id = format!("row-{}", session.id);
                let sql = format!(
                    "INSERT INTO {table} SELECT {values} WHERE ?1 IS NOT NULL AND ?2 IS NOT NULL AND ?3 IS NOT NULL"
                );
                tx.execute(&sql, params![session.id, id, seq])?;
            }
        }
        Ok(())
    })?;
    store.delete_session(&doomed.id)?;
    for (table, _) in rows {
        let count = |session: &str| -> Result<i64> {
            store.read(|conn| {
                Ok(conn.query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE session_id=?1"),
                    [session],
                    |row| row.get(0),
                )?)
            })
        };
        assert_eq!(count(&doomed.id)?, 0, "{table} keeps deleted rows");
        assert_eq!(count(&kept.id)?, 1, "{table} lost surviving rows");
    }
    Ok(())
}
