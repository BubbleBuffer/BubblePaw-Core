//! Uploaded files are stored as file blocks. Rows and queued inputs written before
//! that, with the files as a text envelope, read as the same blocks.
use super::*;
use bubblepaw_core::{ContentBlock, InputFileRef, InputKind, MessageInput};

fn file(id: &str, inline: &str) -> InputFileRef {
    InputFileRef {
        id: id.into(),
        name: format!("{id}.txt"),
        size: inline.len() as u64,
        path: format!(".run/attachments/s/{id}/{id}.txt"),
        url: format!("/api/sessions/s/attachments/{id}"),
        text: true,
        inline: Some(inline.into()),
        omitted: None,
    }
}

/// The text envelope as the host wrote it before file blocks.
fn old_envelope(files: &[InputFileRef]) -> String {
    format!(
        "\n\n<BubblePawAttachments>\n{}\n</BubblePawAttachments>",
        json!({ "files": files })
    )
}

fn blocks(files: &[InputFileRef]) -> Vec<ContentBlock> {
    let mut blocks = vec![ContentBlock::Text {
        text: "look".into(),
    }];
    blocks.extend(files.iter().cloned().map(ContentBlock::file));
    blocks
}

fn raw_rows(path: &std::path::Path) -> Result<Vec<(String, serde_json::Value)>> {
    let conn = Connection::open(path)?;
    let mut statement = conn.prepare("SELECT message_id,data FROM messages ORDER BY seq")?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get::<_, String>(1)?)))?;
    rows.map(|row| {
        let (id, data) = row?;
        Ok((id, serde_json::from_str(&data)?))
    })
    .collect()
}

#[test]
fn file_messages_read_the_same_from_every_stored_form_and_write_blocks() -> Result<()> {
    let big = "猫\n".repeat(3000);
    let files = [file("small", "hi"), file("big", &big)];
    let legacy = format!("look{}", old_envelope(&files));
    let expected = blocks(&files);
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("chat.db");
    let store = Store::open(&path)?;
    let sid = store.create_session("files")?.id;

    // Written: the file blocks sit in the row beside the text, the long text snapshot in a
    // blob, and no envelope anywhere; the model still reads the envelope text.
    let mut written = stored_message(&sid, "written", Role::User, "");
    written.content = expected.clone();
    // Older writes: the envelope in the text, which moves the long text out of the row.
    let old = stored_message(&sid, "old", Role::User, &legacy);
    // Not user messages with a final envelope: text, as written.
    let assistant = stored_message(&sid, "assistant", Role::Assistant, &legacy);
    let trailing = stored_message(&sid, "trailing", Role::User, &format!("{legacy} after"));
    store.append_batch(&[written.clone(), old, assistant, trailing])?;
    let chat = store.chat_path(&sid)?.unwrap();
    store.idle_checkpoint()?;
    let rows = raw_rows(&chat)?;
    let raw = rows[0].1.to_string();
    assert!(!raw.contains("BubblePawAttachments") && !raw.contains(&big));
    assert_eq!(rows[0].1["content"][2]["file"]["id"], "big");
    assert!(rows[0].1["content"][2]["_inline_blob"].is_string());
    assert_eq!(rows[0].1["content"][1]["file"]["inline"], "hi");
    assert!(rows[1].1["_content_parts_blob"].is_string());
    let read = store.messages(&sid)?;
    assert_eq!(read[0].content, expected);
    assert_eq!(read[0].text(), legacy);
    assert_eq!(read[1].content, expected);
    assert_eq!(read[1].text(), legacy);
    for stays_text in &read[2..] {
        assert_eq!(stays_text.files().count(), 0);
        assert_eq!(stays_text.body_text(), stays_text.text());
    }
    drop(store);

    // Older stored forms of the same message.
    let conn = Connection::open(&chat)?;
    for (name, stored) in [
        ("legacy string", json!(legacy)),
        (
            "list, envelope in text",
            json!([{"type":"text","text":legacy}]),
        ),
        ("block form", json!(expected)),
    ] {
        let mut data = rows[0].1.clone();
        data["content"] = stored;
        conn.execute(
            "UPDATE messages SET data=?1 WHERE message_id='written'",
            [data.to_string()],
        )?;
        let reopened = Store::open(&path)?;
        assert_eq!(reopened.messages(&sid)?[0].content, expected, "{name}");
    }
    Ok(())
}

#[tokio::test]
async fn queued_file_inputs_keep_their_blocks_and_old_text_rows_read_as_the_same_blocks()
-> Result<()> {
    let big = "猫\n".repeat(3000);
    let files = [file("a", &big)];
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("chat.db");
    let store = Store::open(&path)?;
    let sid = store.create_session("queued")?.id;
    store.store_attachment(&sid, "a", "{\"id\":\"a\"}", big.as_bytes())?;
    let expected = blocks(&files);

    let plain = store.queue(&sid, InputKind::Followup, "look")?;
    let new = store.queue(
        &sid,
        InputKind::Followup,
        MessageInput::from(expected.clone()),
    )?;
    let old = store.queue(
        &sid,
        InputKind::Followup,
        format!("look{}", old_envelope(&files)),
    )?;
    let conn = Connection::open(store.chat_path(&sid)?.unwrap())?;
    let format_of = |id: &str| -> Result<Option<String>> {
        Ok(conn.query_row(
            "SELECT content_format FROM inputs WHERE id=?1",
            [id],
            |row| row.get(0),
        )?)
    };
    assert_eq!(format_of(&new.id)?.as_deref(), Some("blocks"));
    assert_ne!(format_of(&old.id)?.as_deref(), Some("blocks"));
    // The attachment, the plain text, the old input's parts list and the new input's block list
    // share the long text in one blob.
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get::<_, i64>(0))?,
        4
    );
    let queued = store.queued_inputs(&sid)?;
    assert_eq!(queued[0].id, plain.id);
    assert_eq!(queued[0].content, bubblepaw_core::text_blocks("look"));
    for input in &queued[1..] {
        assert_eq!(input.content, expected, "{}", input.id);
        assert_eq!(input.text(), "look");
    }
    let json = serde_json::to_value(&queued[1])?;
    assert_eq!(json["text"], "look");
    assert_eq!(json["content"], serde_json::to_value(&expected)?);
    assert!(serde_json::to_value(&queued[0])?.get("content").is_none());

    // Consumed, each becomes a user message with the same blocks; a body already stored as a
    // blob is shared while the files stay in the row.
    input_harness(&store)
        .resume(&sid, Default::default())
        .await?;
    let messages = store.messages(&sid)?;
    let users: Vec<_> = messages.iter().filter(|m| m.role == Role::User).collect();
    assert_eq!(users.len(), 3);
    assert_eq!(users[0].content, bubblepaw_core::text_blocks("look"));
    assert_eq!(users[1].content, expected);
    assert_eq!(users[2].content, expected);
    assert!(
        users[1]
            .text()
            .contains(&serde_json::to_string(&big)?[1..100])
    );
    Ok(())
}
