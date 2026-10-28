//! Numbered store migrations. A store the release (e67bd80) could produce still
//! opens, with its data intact, whether or not the old per-open upgrade had already
//! applied the change to it.
use anyhow::Result;
use bubblepaw_core::Store;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;
use std::path::PathBuf;

const DATE: &str = "2026-10-01T00:00:00Z";
/// A request row beyond every retention cutoff, so housekeeping keeps it.
const LATER: &str = "2099-01-01T00:00:00Z";
const OLD_READING: &str = r#"{"provider":"p","account":"work","window":"weekly","used_percent":80.0,"resets_at":"2026-10-21T00:00:00Z","observed_at":"2026-10-20T19:00:00Z","source":"response_headers"}"#;

const CATALOG_V1: &str = include_str!("fixtures/catalog-v1-schema.sql");
const SERVING_V1: &str = include_str!("fixtures/serving-v1-schema.sql");
/// The release's v9 chat record: numbered steps 1 to 9, then the tables its
/// per-open code created and never versioned.
const CHAT_V9: &str = concat!(
    include_str!("fixtures/main-v6-schema.sql"),
    include_str!("fixtures/v7-additions.sql"),
    include_str!("fixtures/chat-v8-v9-additions.sql"),
    include_str!("fixtures/chat-v9-per-open-schema.sql"),
);

/// The request-log columns added after the first release, by serving step 2.
const REQUEST_COLUMNS: [&str; 7] = [
    "cache_write_tokens",
    "output_tokens",
    "reasoning_tokens",
    "reasoning_effort",
    "breakpoints",
    "cache_lifetime_ms",
    "likely_expired",
];
/// Serving step 2, as the release's per-open code ran it.
const ADDED_REQUEST_COLUMNS: &str =
    "ALTER TABLE model_requests ADD COLUMN cache_write_tokens INTEGER;
ALTER TABLE model_requests ADD COLUMN output_tokens INTEGER;
ALTER TABLE model_requests ADD COLUMN reasoning_tokens INTEGER;
ALTER TABLE model_requests ADD COLUMN reasoning_effort TEXT;
ALTER TABLE model_requests ADD COLUMN breakpoints TEXT;
ALTER TABLE model_requests ADD COLUMN cache_lifetime_ms INTEGER;
ALTER TABLE model_requests ADD COLUMN likely_expired INTEGER;";
/// Record step 10, as the release's per-open code ran it.
const RELAXED_CHECKPOINTS: &str = "CREATE TABLE checkpoint_records_relaxed(session_id TEXT NOT NULL REFERENCES sessions(id),id TEXT NOT NULL,boundary_seq INTEGER NOT NULL,metadata TEXT NOT NULL,preimage_hash TEXT REFERENCES checkpoint_blobs(hash),PRIMARY KEY(session_id,id));
INSERT INTO checkpoint_records_relaxed SELECT session_id,id,boundary_seq,metadata,preimage_hash FROM checkpoint_records;
DROP TABLE checkpoint_records;
ALTER TABLE checkpoint_records_relaxed RENAME TO checkpoint_records;
CREATE INDEX checkpoint_records_preimage ON checkpoint_records(preimage_hash);
CREATE INDEX checkpoint_records_boundary ON checkpoint_records(session_id,boundary_seq);";
/// Record step 11, as the release's per-open code ran it.
const KINDED_TOOL_CALLS: &str = "DROP TABLE IF EXISTS message_tool_calls;
CREATE TABLE message_tool_calls(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,call_id TEXT NOT NULL,message_seq INTEGER NOT NULL REFERENCES messages(seq) ON DELETE CASCADE,kind TEXT NOT NULL CHECK(kind IN ('call','result')),PRIMARY KEY(session_id,call_id,message_seq,kind));
CREATE INDEX message_tool_calls_sequence ON message_tool_calls(session_id,message_seq);";

fn version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn count(conn: &Connection, sql: &str) -> Result<i64> {
    Ok(conn.query_row(sql, [], |row| row.get(0))?)
}

fn text(conn: &Connection, sql: &str) -> Result<String> {
    Ok(conn.query_row(sql, [], |row| row.get(0))?)
}

/// Some(`NOT NULL`) of the named column, or None when the table lacks it.
fn column(conn: &Connection, table: &str, name: &str) -> Result<Option<bool>> {
    let sql = format!("SELECT \"notnull\"=1 FROM pragma_table_info('{table}') WHERE name=?1");
    Ok(conn.query_row(&sql, [name], |row| row.get(0)).optional()?)
}

#[test]
fn old_style_catalog_opens() -> Result<()> {
    for applied in [false, true] {
        let dir = tempfile::tempdir()?;
        let catalog = dir.path().join("catalog.db");
        let conn = Connection::open(&catalog)?;
        conn.execute_batch(CATALOG_V1)?;
        conn.execute(
            "INSERT INTO quota_history(provider,account,\"window\",data) VALUES('p','work','weekly',?1)",
            [OLD_READING],
        )?;
        if applied {
            // A quota write of the release added the column without a version bump.
            conn.execute_batch("ALTER TABLE quota_history ADD COLUMN balance TEXT;")?;
        }
        assert_eq!(version(&conn)?, 1);
        drop(conn);

        let store = Store::open_data_dir(dir.path())?;
        let conn = Connection::open(&catalog)?;
        assert_eq!(version(&conn)?, 2, "catalog step 2");
        assert_eq!(column(&conn, "quota_history", "balance")?, Some(false));
        assert_eq!(text(&conn, "SELECT data FROM quota_history")?, OLD_READING);
        // Old rows keep a null balance.
        let nulls = "SELECT COUNT(*) FROM quota_history WHERE balance IS NULL";
        assert_eq!(count(&conn, nulls)?, 1);
        drop(conn);
        drop(store);

        // A later open migrates nothing and keeps the row.
        drop(Store::open_data_dir(dir.path())?);
        let conn = Connection::open(&catalog)?;
        assert_eq!(version(&conn)?, 2);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM quota_history")?, 1);
    }
    Ok(())
}

#[test]
fn old_style_serving_opens() -> Result<()> {
    for applied in [false, true] {
        let dir = tempfile::tempdir()?;
        let serving = dir.path().join("serving.db");
        let conn = Connection::open(&serving)?;
        conn.execute_batch(SERVING_V1)?;
        conn.execute(
            "INSERT INTO model_requests(session_id,lineage,seq,created_at,frozen_sha256,frozen_bytes,cold_sha256,cold_bytes,cold_count,history_sha256,history_bytes,history_count,classification,declared,provider,model,warned) VALUES('old-session','lineage',1,?1,'frozen',0,'cold',0,0,'history',0,0,'cold',0,'provider','model',0)",
            [LATER],
        )?;
        if applied {
            conn.execute_batch(ADDED_REQUEST_COLUMNS)?;
        }
        assert_eq!(version(&conn)?, 1);
        drop(conn);

        let store = Store::open_data_dir(dir.path())?;
        let conn = Connection::open(&serving)?;
        assert_eq!(version(&conn)?, 2, "serving step 2");
        for name in REQUEST_COLUMNS {
            let present = column(&conn, "model_requests", name)?;
            assert_eq!(present, Some(false), "missing {name}");
        }
        let requests = "SELECT COUNT(*) FROM model_requests";
        assert_eq!(count(&conn, requests)?, 1);
        drop(conn);
        drop(store);

        // A later open migrates nothing and keeps the row.
        drop(Store::open_data_dir(dir.path())?);
        let conn = Connection::open(&serving)?;
        assert_eq!(version(&conn)?, 2);
        assert_eq!(count(&conn, requests)?, 1);
    }
    Ok(())
}

#[test]
fn old_style_chat_checkpoint_opens() -> Result<()> {
    for applied in [false, true] {
        let dir = tempfile::tempdir()?;
        let store = Store::open_data_dir(dir.path())?;
        let session = store.create_session("old style")?;
        let chat = store.chat_path(&session.id)?.unwrap();
        drop(store);
        // Replace the fresh record with the fixture: the release's v9 file.
        for suffix in ["", "-wal", "-shm"] {
            let path = PathBuf::from(format!("{}{suffix}", chat.display()));
            if path.exists() {
                std::fs::remove_file(path)?;
            }
        }
        let conn = Connection::open(&chat)?;
        conn.execute_batch(CHAT_V9)?;
        let message = json!({
            "id": "old-message", "session_id": session.id, "role": "user",
            "content": "original history", "created_at": DATE,
        });
        let session_data = serde_json::to_string(&session)?;
        conn.execute(
            "INSERT INTO sessions(id,data) VALUES(?1,?2)",
            params![session.id, session_data],
        )?;
        conn.execute(
            "INSERT INTO messages(session_id,message_id,data) VALUES(?1,'old-message',?2)",
            params![session.id, message.to_string()],
        )?;
        if applied {
            conn.execute_batch(RELAXED_CHECKPOINTS)?;
            conn.execute_batch(KINDED_TOOL_CALLS)?;
        }
        conn.execute(
            "INSERT INTO checkpoint_blobs(hash,raw_bytes,data) VALUES('preimage',4,x'00000000')",
            [],
        )?;
        conn.execute(
            "INSERT INTO checkpoint_records(session_id,id,boundary_seq,metadata,preimage_hash) VALUES(?1,'checkpoint/one',1,'{}','preimage')",
            [&session.id],
        )?;
        assert_eq!(version(&conn)?, 9);
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        drop(conn);

        let store = Store::open_data_dir(dir.path())?;
        let messages = store.messages(&session.id)?;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "original history");
        let conn = Connection::open(&chat)?;
        assert_eq!(version(&conn)?, 12, "record steps 10 to 12");
        assert_eq!(column(&conn, "message_tool_calls", "kind")?, Some(true));
        // Step 11 also builds the derived headers table from the same message.
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM message_headers")?, 1);
        let nullable = column(&conn, "checkpoint_records", "preimage_hash")?;
        assert_eq!(nullable, Some(false));
        let preimage = "SELECT preimage_hash FROM checkpoint_records WHERE id='checkpoint/one'";
        assert_eq!(text(&conn, preimage)?, "preimage");
        drop(conn);
        drop(store);

        // A later open rewrites nothing and keeps the record readable.
        let reopened = Store::open_data_dir(dir.path())?;
        assert_eq!(reopened.messages(&session.id)?.len(), 1);
        let conn = Connection::open(&chat)?;
        assert_eq!(version(&conn)?, 12);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM checkpoint_records")?, 1);
    }
    Ok(())
}
