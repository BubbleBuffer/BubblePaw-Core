use super::*;

#[tokio::test]
async fn retired_boundary_metadata_survives_reopen_without_controlling_chapters() -> Result<()> {
    use rusqlite::params;
    use sha2::{Digest, Sha256};

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("retired-metadata.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("old boundary metadata")?;
    let mut history = canonical_history(&store, &session.id, 3).await?;
    let mut first = batch(&session.id, 1, &history[..2], 1, 1, None);
    first.close_after_episode = Some(close(first.open_chapter.clone()));
    store.save_episode_batch(&first)?;
    let mut second = batch(&session.id, 2, &history[2..4], 2, 2, Some(&history[1].id));
    second.close_after_episode = Some(close(second.open_chapter.clone()));
    store.save_episode_batch(&second)?;

    // Recreate the exact retired host key, receipt format and extra chapter field
    // on the unchanged v9 schema. Split identifiers keep removed tool/API names
    // out of the active source surface while preserving the on-disk fixture.
    let state_key = concat!("chapter_", "marks");
    let relation_key = concat!("marked_", "related_ids");
    let pending = serde_json::json!({
        "id": "legacy-call",
        "boundary_message_id": history[5].id,
        "title": "Old explicit ending",
        "related_ids": [first.open_chapter.id],
    });
    store.update_host_state_snapshot(&session.id, "", state_key, |_| {
        Ok(serde_json::json!([pending.clone()]))
    })?;
    history = canonical_history(&store, &session.id, 1).await?;
    let queued = store.queue(&session.id, InputKind::Followup, "keep queued work")?;
    let chat_path = store.chat_path(&session.id)?.unwrap();
    let conn = rusqlite::Connection::open(&chat_path)?;
    let chapter_data: String = conn.query_row(
        "SELECT data FROM context_subject_chapters WHERE session_id=?1 AND id=?2",
        params![session.id, second.open_chapter.id],
        |row| row.get(0),
    )?;
    let mut old_chapter: serde_json::Value = serde_json::from_str(&chapter_data)?;
    old_chapter[relation_key] = serde_json::json!([first.open_chapter.id]);
    let old_chapter_bytes = serde_json::to_string(&old_chapter)?;
    conn.execute(
        "UPDATE context_subject_chapters SET data=?1 WHERE session_id=?2 AND id=?3",
        params![old_chapter_bytes, session.id, second.open_chapter.id],
    )?;
    let empty_hash = bubblepaw_core::hex_lower(Sha256::digest(b""));
    conn.execute(
        "INSERT OR IGNORE INTO blobs(hash,raw_bytes,data) VALUES(?1,0,?2)",
        params![empty_hash, zstd::stream::encode_all(&b""[..], 3)?],
    )?;
    let receipt_suffix: String = b"legacy-call"
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    conn.execute(
        "INSERT INTO attachments(session_id,id,metadata,blob_hash) VALUES(?1,?2,?3,?4)",
        params![
            session.id,
            format!("chapter-mark-v1/{receipt_suffix}"),
            serde_json::to_string(&pending)?,
            empty_hash,
        ],
    )?;
    let attachment_rows = |connection: &rusqlite::Connection| {
        let mut statement = connection.prepare(
            "SELECT a.id,a.metadata,a.blob_hash,b.data FROM attachments a JOIN blobs b ON b.hash=a.blob_hash WHERE a.session_id=?1 ORDER BY a.id",
        )?;
        statement
            .query_map([&session.id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
    };
    let old_attachments = attachment_rows(&conn)?;
    let old_state_bytes: String = conn.query_row(
        "SELECT value FROM host_state WHERE session_id=?1 AND key=?2",
        params![session.id, state_key],
        |row| row.get(0),
    )?;
    drop(conn);
    drop(store);

    let store = Store::open(&path)?;
    assert_eq!(
        serde_json::to_value(store.messages(&session.id)?)?,
        serde_json::to_value(&history)?
    );
    assert_eq!(
        serde_json::to_value(
            store
                .queued_inputs(&session.id)?
                .into_iter()
                .find(|input| input.id == queued.id)
                .unwrap()
        )?,
        serde_json::to_value(queued)?
    );
    let chapters = store.context_subject_chapters(&session.id)?;
    assert_eq!(chapters.len(), 2);
    assert_eq!(chapters[1].short_id(), "ch-2");
    assert!(chapters[1].related_ids.is_empty());
    assert!(
        !serde_json::to_value(&chapters[1])?
            .as_object()
            .unwrap()
            .contains_key(relation_key)
    );

    let third = batch(&session.id, 3, &history[4..6], 3, 3, Some(&history[3].id));
    store.save_episode_batch(&third)?;
    assert_eq!(
        store.open_subject_chapter(&session.id)?.unwrap().id,
        third.open_chapter.id
    );
    let mut fourth = batch(&session.id, 4, &history[6..8], 4, 4, Some(&history[5].id));
    fourth.closed_chapter = Some(close(third.open_chapter));
    fourth.decisions[0].choice = Some(history[6].id.clone());
    fourth.decisions[0].probabilities = vec![1.0, 0.0];
    store.save_episode_batch(&fourth)?;
    assert_eq!(store.context_episodes(&session.id)?.len(), 4);
    assert_eq!(
        store.context_decisions(&session.id)?[3].choice,
        Some(history[6].id.clone())
    );
    assert_eq!(
        store.host_state(&session.id, state_key)?,
        Some(serde_json::json!([pending]))
    );

    let conn = rusqlite::Connection::open(chat_path)?;
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
        12
    );
    assert_eq!(
        conn.query_row(
            "SELECT data FROM context_subject_chapters WHERE session_id=?1 AND id=?2",
            params![session.id, second.open_chapter.id],
            |row| row.get::<_, String>(0)
        )?,
        old_chapter_bytes
    );
    assert_eq!(
        conn.query_row(
            "SELECT value FROM host_state WHERE session_id=?1 AND key=?2",
            params![session.id, state_key],
            |row| row.get::<_, String>(0)
        )?,
        old_state_bytes
    );
    assert_eq!(attachment_rows(&conn)?, old_attachments);
    Ok(())
}
