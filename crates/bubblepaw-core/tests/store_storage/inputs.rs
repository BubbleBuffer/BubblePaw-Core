use super::*;

#[tokio::test]
async fn accepted_input_transfers_one_blob_from_pending_queue_to_canonical_message() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("accepted.db");
    let store = Store::open(&path)?;
    let session = store.create_session("accepted")?;
    let text = "Small accepted input containing Unicode 猫 and\nnewlines.";
    let input = store.queue(&session.id, bubblepaw_core::InputKind::Followup, text)?;
    let conn = Connection::open(store.chat_path(&session.id)?.unwrap())?;
    let hash: String = conn.query_row(
        "SELECT content_blob FROM inputs WHERE id=?1",
        [&input.id],
        |row| row.get(0),
    )?;
    assert_eq!(
        conn.query_row("SELECT text FROM inputs WHERE id=?1", [&input.id], |row| {
            row.get::<_, String>(0)
        })?,
        ""
    );
    assert_eq!(store.queued_inputs(&session.id)?[0].text, text);
    assert_eq!(
        store.snapshot(&session.id, 10, 10, 10)?.pending_inputs[0].text,
        text
    );
    input_harness(&store)
        .resume(&session.id, Default::default())
        .await?;
    let data: String = conn.query_row(
        "SELECT data FROM messages WHERE json_extract(data,'$.role')='user'",
        [],
        |row| row.get(0),
    )?;
    let value: serde_json::Value = serde_json::from_str(&data)?;
    assert_eq!(value["content"], "");
    assert_eq!(value["_content_blob"], hash);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get::<_, i64>(0))?,
        1
    );
    store.flush_serving()?;
    let serving = Connection::open(Store::legacy_data_dir(&path).join("serving.db"))?;
    assert!(
        serving
            .query_row(
                "SELECT content_blob FROM inputs WHERE id=?1",
                [&input.id],
                |row| row.get::<_, Option<String>>(0)
            )?
            .is_none()
    );
    assert!(store.queued_inputs(&session.id)?[0].text.is_empty());
    assert_eq!(store.messages(&session.id)?[0].content, text);
    drop(conn);
    drop(store);
    assert_eq!(Store::open(&path)?.messages(&session.id)?[0].content, text);
    Ok(())
}
