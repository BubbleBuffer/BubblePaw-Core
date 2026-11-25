//! Message rows are written in the list form and read in both forms.
use super::*;

fn rows(path: &std::path::Path) -> Result<Vec<(i64, serde_json::Value)>> {
    let conn = Connection::open(path)?;
    let mut statement = conn.prepare("SELECT seq, data FROM messages ORDER BY seq")?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get::<_, String>(1)?)))?;
    rows.map(|row| {
        let (seq, data) = row?;
        Ok((seq, serde_json::from_str(&data)?))
    })
    .collect()
}

#[test]
fn rows_are_written_as_block_lists_and_old_string_rows_still_read_back() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open(directory.path().join("chat.db"))?;
    let session = store.create_session("forms")?;
    let sid = &session.id;
    let file = "猫\n".repeat(3000);
    store.store_attachment(sid, "f", "{\"id\":\"f\"}", file.as_bytes())?;
    let inline = json!({"files":[{"id":"f","name":"n.txt","size":file.len(),"inline":file}]});
    let attached = format!("See.\n\n<BubblePawAttachments>\n{inline}\n</BubblePawAttachments>");
    let mut failed = stored_message(sid, "failed", Role::Tool, "small error");
    failed.is_error = true;
    let messages = [
        stored_message(sid, "plain", Role::User, "plain text"),
        stored_message(sid, "large", Role::Tool, &"x".repeat(5000)),
        stored_message(sid, "attached", Role::User, &attached),
        failed,
        stored_message(sid, "repeat", Role::User, "small error"),
    ];
    store.append_batch(&messages)?;
    let path = store.chat_path(sid)?.unwrap();

    // New rows: the list form inline, an empty list beside each blob key.
    let written = rows(&path)?;
    assert_eq!(
        written[0].1["content"],
        json!([{"type":"text","text":"plain text"}])
    );
    for (_, data) in &written[1..] {
        assert_eq!(data["content"], json!([]));
    }
    for (index, key) in [(1, "_content_blob"), (2, "_content_parts_blob")] {
        assert!(written[index].1[key].is_string(), "{key}");
    }
    assert_eq!(written[3].1["_content_blob"], written[4].1["_content_blob"]);
    let read = |store: &Store| -> Result<Vec<String>> {
        Ok(store
            .messages(sid)?
            .iter()
            .map(|m| m.text().into_owned())
            .collect())
    };
    let texts: Vec<String> = messages.iter().map(|m| m.text().into_owned()).collect();
    assert_eq!(read(&store)?, texts);
    drop(store);

    // Old rows: the text as a plain string, an empty string beside a blob key.
    let conn = Connection::open(&path)?;
    for (seq, mut data) in written {
        let text = data["content"][0]["text"].as_str().unwrap_or("").to_owned();
        data["content"] = json!(text);
        conn.execute(
            "UPDATE messages SET data=?1 WHERE seq=?2",
            (data.to_string(), seq),
        )?;
    }
    drop(conn);
    let store = Store::open(directory.path().join("chat.db"))?;
    assert_eq!(read(&store)?, texts);
    Ok(())
}
