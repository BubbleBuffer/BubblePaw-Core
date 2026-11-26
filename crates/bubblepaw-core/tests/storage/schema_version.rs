//! Each store file is created at its final schema. A file with no chain of
//! upgrade steps to the current version (older), or written by a newer build,
//! fails with `StoreSchemaError` and is left as it was.
use anyhow::Result;
use bubblepaw_core::{ContentBlock, ImageRef, Message, Role, Store, StoreSchemaError};
use rusqlite::Connection;
use serde_json::json;
use std::path::{Path, PathBuf};

fn message(session: &str) -> Message {
    Message {
        id: "m0".into(),
        session_id: session.into(),
        role: Role::User,
        origin: None,
        content: bubblepaw_core::text_blocks("kept across restart"),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        details: None,
        created_at: "2026-10-30T00:00:00Z".into(),
    }
}

fn pragma(path: &Path, name: &str) -> Result<i64> {
    Ok(Connection::open(path)?.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))?)
}

fn set_version(path: &Path, version: i64) -> Result<()> {
    Connection::open(path)?.execute_batch(&format!("PRAGMA user_version={version};"))?;
    Ok(())
}

/// A data directory with one session and message, and the paths of its three files.
type StoreFile = (&'static str, PathBuf, i64);
fn populated(dir: &Path) -> Result<(String, [StoreFile; 3])> {
    let store = Store::open_data_dir(dir)?;
    let session = store.create_session("versioned")?;
    store.append_batch(&[message(&session.id)])?;
    let chat = store.chat_path(&session.id)?.expect("persistent chat file");
    drop(store);
    Ok((
        session.id,
        [
            ("chat", chat, 15),
            ("catalog", dir.join("catalog.db"), 2),
            ("serving", dir.join("serving.db"), 2),
        ],
    ))
}

/// Open the directory and read the session, returning the first error.
fn open_and_read(dir: &Path, session: &str) -> Result<()> {
    let store = Store::open_data_dir(dir)?;
    store.messages(session)?;
    Ok(())
}

#[test]
fn a_fresh_store_reopens_with_its_data_and_the_same_versions() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (session, files) = populated(dir.path())?;
    let before: Vec<(i64, i64)> = files
        .iter()
        .map(|(_, path, _)| {
            Ok((
                pragma(path, "application_id")?,
                pragma(path, "user_version")?,
            ))
        })
        .collect::<Result<_>>()?;
    for ((name, _, version), (application, found)) in files.iter().zip(&before) {
        assert_eq!(found, version, "{name} is created at its final version");
        assert_ne!(*application, 0, "{name} carries its application id");
    }

    let store = Store::open_data_dir(dir.path())?;
    let messages = store.messages(&session)?;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text(), "kept across restart");
    drop(store);

    let after: Vec<(i64, i64)> = files
        .iter()
        .map(|(_, path, _)| {
            Ok((
                pragma(path, "application_id")?,
                pragma(path, "user_version")?,
            ))
        })
        .collect::<Result<_>>()?;
    assert_eq!(before, after);
    Ok(())
}

#[test]
fn a_store_at_another_version_fails_with_a_typed_error_and_is_not_modified() -> Result<()> {
    for (index, name) in ["chat", "catalog", "serving"].into_iter().enumerate() {
        // The chat store has upgrade steps from 12 to 14, so its older case is 11.
        for shift in [if name == "chat" { -4_i64 } else { -1 }, 1] {
            let dir = tempfile::tempdir()?;
            let (session, files) = populated(dir.path())?;
            let (_, path, expected) = &files[index];
            let found = expected + shift;
            set_version(path, found)?;
            let bytes = std::fs::read(path)?;

            let error = open_and_read(dir.path(), &session)
                .expect_err(&format!("{name} at version {found} must not open"));
            let typed = error
                .downcast_ref::<StoreSchemaError>()
                .unwrap_or_else(|| panic!("{name} at {found}: not a StoreSchemaError: {error:#}"));
            assert_eq!(
                (typed.store, typed.found, typed.expected),
                (name, found, *expected),
                "{error:#}"
            );
            let wording = if shift > 0 {
                "newer"
            } else {
                "no upgrade path"
            };
            assert!(
                error.to_string().contains(wording),
                "the message says {wording}: {error}"
            );
            assert_eq!(pragma(path, "user_version")?, found, "{name} version kept");
            assert_eq!(std::fs::read(path)?, bytes, "{name} file not modified");
        }
    }
    Ok(())
}

#[test]
fn a_foreign_database_is_still_refused_as_foreign() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (session, files) = populated(dir.path())?;
    let (_, chat, _) = &files[0];
    Connection::open(chat)?.execute_batch("PRAGMA application_id=7;")?;
    let error = open_and_read(dir.path(), &session).expect_err("foreign chat must not open");
    assert!(
        error.downcast_ref::<StoreSchemaError>().is_none(),
        "{error:#}"
    );
    Ok(())
}

#[test]
fn chat_stores_before_15_open_at_15_and_keep_their_messages() -> Result<()> {
    for from in [12, 13, 14] {
        let dir = tempfile::tempdir()?;
        let (session, files) = populated(dir.path())?;
        let chat = &files[0].1;
        let conn = Connection::open(chat)?;
        let data: String = conn.query_row("SELECT data FROM messages", [], |row| row.get(0))?;
        let mut data: serde_json::Value = serde_json::from_str(&data)?;
        data["content"] = "kept across restart".into();
        conn.execute("UPDATE messages SET data=?1", [data.to_string()])?;
        drop(conn);
        set_version(chat, from)?;

        let store = Store::open_data_dir(dir.path())?;
        assert_eq!(store.messages(&session)?[0].text(), "kept across restart");
        drop(store);
        assert_eq!(pragma(chat, "user_version")?, 15, "from {from}");
    }
    Ok(())
}

fn raw_message(chat: &Path) -> Result<serde_json::Value> {
    let conn = Connection::open(chat)?;
    let data: String = conn.query_row("SELECT data FROM messages", [], |row| row.get(0))?;
    Ok(serde_json::from_str(&data)?)
}

/// Every stored form of an image message reads as the same blocks, and a write
/// stores the block form: the image beside its text, never an envelope in it.
#[test]
fn image_messages_read_the_same_from_every_stored_form_and_write_blocks() -> Result<()> {
    let image = ImageRef {
        id: format!("image-{:064x}", 7),
        session_id: "s".into(),
        mime_type: "image/png".into(),
        bytes: 12,
    };
    let envelope = format!(
        "\n<BubblePawImages>\n{}\n</BubblePawImages>",
        json!({"images":[image]})
    );
    let block = json!({"type":"image","image":image});
    let expected = vec![
        ContentBlock::Text {
            text: "look".into(),
        },
        ContentBlock::image(image.clone()),
    ];
    for (name, stored) in [
        ("legacy string", json!(format!("look{envelope}"))),
        (
            "list, envelope in text",
            json!([{"type":"text","text":format!("look{envelope}")}]),
        ),
        ("block form", json!([{"type":"text","text":"look"}, block])),
    ] {
        let dir = tempfile::tempdir()?;
        let (session, files) = populated(dir.path())?;
        let chat = &files[0].1;
        let conn = Connection::open(chat)?;
        let mut data = raw_message(chat)?;
        data["content"] = stored;
        data["role"] = json!("tool");
        conn.execute("UPDATE messages SET data=?1", [data.to_string()])?;
        drop(conn);
        let store = Store::open_data_dir(dir.path())?;
        assert_eq!(store.messages(&session)?[0].content, expected, "{name}");
    }

    // Only tool messages carry the legacy envelope: a user's text is kept as written.
    let dir = tempfile::tempdir()?;
    let (session, files) = populated(dir.path())?;
    let conn = Connection::open(&files[0].1)?;
    let mut data = raw_message(&files[0].1)?;
    data["content"] = json!(format!("look{envelope}"));
    conn.execute("UPDATE messages SET data=?1", [data.to_string()])?;
    drop(conn);
    let store = Store::open_data_dir(dir.path())?;
    let user = store.messages(&session)?.remove(0);
    assert_eq!(user.role, Role::User);
    assert_eq!(user.text(), format!("look{envelope}"));
    assert_eq!(user.images().count(), 0);

    // A write keeps the image block in the row, also when the text moves to a blob.
    for (name, text) in [("small", "look".to_owned()), ("large", "x".repeat(5000))] {
        let dir = tempfile::tempdir()?;
        let (session, files) = populated(dir.path())?;
        let mut tool = message(&session);
        tool.id = format!("tool-{name}");
        tool.role = Role::Tool;
        tool.tool_call_id = Some("call".into());
        tool.content = vec![
            ContentBlock::Text { text: text.clone() },
            ContentBlock::image_with_data(image.clone(), vec![1u8, 2, 3]),
        ];
        let store = Store::open_data_dir(dir.path())?;
        store.append_batch(&[tool])?;
        let chat = &files[0].1;
        store.idle_checkpoint()?;
        let conn = Connection::open(chat)?;
        let raw: String = conn.query_row(
            "SELECT data FROM messages WHERE message_id=?1",
            [format!("tool-{name}")],
            |row| row.get(0),
        )?;
        let raw: serde_json::Value = serde_json::from_str(&raw)?;
        assert!(!raw.to_string().contains("BubblePawImages"), "{name}");
        assert!(
            raw["content"].as_array().unwrap().contains(&block),
            "{name}: {raw}"
        );
        assert!(
            !raw.to_string().contains("AQID"),
            "{name}: bytes are never stored"
        );
        let read = store
            .messages(&session)?
            .into_iter()
            .find(|message| message.role == Role::Tool)
            .unwrap();
        assert_eq!(read.text(), text, "{name}");
        assert_eq!(
            read.content.last(),
            Some(&ContentBlock::image(image.clone())),
            "{name}"
        );
    }
    Ok(())
}
