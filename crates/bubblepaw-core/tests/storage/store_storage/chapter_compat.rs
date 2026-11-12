//! Chapter metadata compatibility keeps the existing test coverage unchanged.
use super::*;

#[tokio::test]
async fn attachment_bytes_and_inline_json_use_one_blob_with_exact_escaped_roundtrip() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("attachments.db");
    let store = Store::open(&path)?;
    let session = store.create_session("attachments")?;
    let text = "Text with 猫, quotes \"like this\", a slash \\ and\nnewlines.\nLiteral tags: <BubblePawAttachments> and </BubblePawAttachments>.\n".repeat(500);
    let metadata = "{\"id\":\"file-1\",\"name\":\"notes.txt\",\"text\":true}";
    let hash = store.store_attachment(&session.id, "file-1", metadata, text.as_bytes())?;
    assert_eq!(
        store.store_attachment(&session.id, "file-1", metadata, text.as_bytes())?,
        hash
    );
    assert!(
        store
            .store_attachment(&session.id, "file-1", metadata, b"replacement")
            .is_err()
    );
    assert!(
        store
            .store_attachment(&session.id, "file-1", "{}", text.as_bytes())
            .is_err()
    );
    let inline =
        json!({"files":[{"id":"file-1","name":"notes.txt","size":text.len(),"inline":text}]});
    let content =
        format!("Please inspect.\n\n<BubblePawAttachments>\n{inline}\n</BubblePawAttachments>");
    store.queue(&session.id, bubblepaw_core::InputKind::Followup, &content)?;
    assert_eq!(store.queued_inputs(&session.id)?[0].text, content);
    assert_eq!(
        store.snapshot(&session.id, 10, 10, 10)?.pending_inputs[0].text,
        content
    );
    let conn = Connection::open(store.chat_path(&session.id)?.unwrap())?;
    let descriptor: String =
        conn.query_row("SELECT content_blob FROM inputs", [], |row| row.get(0))?;
    assert_eq!(
        conn.query_row("SELECT text FROM inputs", [], |row| row.get::<_, String>(0))?,
        ""
    );
    let agent = input_harness(&store);
    agent.resume(&session.id, Default::default()).await?;
    assert_eq!(
        store.attachment(&session.id, "file-1")?,
        Some((metadata.into(), text.as_bytes().to_vec()))
    );
    assert!(store.attachment(&session.id, "missing")?.is_none());
    assert_eq!(store.messages(&session.id)?[0].content, content);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get::<_, i64>(0))?,
        2
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM blobs WHERE hash=?1", [&hash], |row| {
            row.get::<_, i64>(0)
        })?,
        1
    );
    assert_eq!(
        conn.query_row("SELECT blob_hash FROM attachments", [], |row| row
            .get::<_, String>(0))?,
        hash
    );
    let data: String = conn.query_row(
        "SELECT data FROM messages WHERE json_extract(data,'$.role')='user'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&data)?["_content_parts_blob"],
        descriptor
    );
    assert!(store.queued_inputs(&session.id)?[0].text.is_empty());
    assert!(!data.contains(&text));
    drop(conn);
    drop(agent);
    drop(store);
    let reopened = Store::open(&path)?;
    assert_eq!(reopened.messages(&session.id)?[0].content, content);
    assert_eq!(
        reopened.attachment(&session.id, "file-1")?,
        Some((metadata.into(), text.into_bytes()))
    );
    Ok(())
}
