//! Each store file is created at its final schema and opens only at exactly that
//! version. A file at any other version fails with `StoreSchemaError` and is left
//! as it was.
use anyhow::Result;
use bubblepaw_core::{Message, Role, Store, StoreSchemaError};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

fn message(session: &str) -> Message {
    Message {
        id: "m0".into(),
        session_id: session.into(),
        role: Role::User,
        origin: None,
        content: "kept across restart".into(),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
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
            ("chat", chat, 12),
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
    assert_eq!(messages[0].content, "kept across restart");
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
        for shift in [-1_i64, 1] {
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
            assert!(
                error.to_string().contains("no upgrade path"),
                "the message says there is no upgrade path: {error}"
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
