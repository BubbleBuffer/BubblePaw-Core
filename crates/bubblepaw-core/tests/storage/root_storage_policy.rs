use anyhow::Result;
use bubblepaw_core::{
    ContextCheckpoint, ContextStrategy, Message, Role, RootStoragePolicy, Store, StoreOptions,
};
use rusqlite::{Connection, params};
use serde_json::json;

fn policy(hours: u32, days: u32) -> RootStoragePolicy {
    RootStoragePolicy {
        store: StoreOptions {
            finished_event_retention_hours: hours,
            telemetry_retention_days: days,
            archive_after_days: 0,
            ..Default::default()
        },
        archived_retention_days: None,
    }
}

#[test]
fn root_policy_is_atomic_inherited_and_rebuilt_without_chat_rewrites() -> Result<()> {
    let data = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let store = Store::open_data_dir(data.path())?;
    let saved = policy(1, 2);
    let root = store.create_session_with_policy(
        "configured",
        ContextStrategy::default(),
        workspace.path(),
        saved,
    )?;
    let old = store.create_session("legacy fallback")?;
    let child =
        store.create_session_with_parent("child", ContextStrategy::default(), Some(&root.id))?;
    assert_eq!(store.session_storage_policy(&child.id)?, Some(saved));
    assert_eq!(store.session_storage_policy(&old.id)?, None);
    let path = store.chat_path(&root.id)?.unwrap();
    let invalid = RootStoragePolicy {
        archived_retention_days: Some(u32::MAX),
        ..saved
    };
    assert!(
        store
            .create_session_with_policy(
                "invalid",
                ContextStrategy::default(),
                workspace.path(),
                invalid
            )
            .is_err()
    );
    assert_eq!(store.list_sessions()?.len(), 3);
    drop(store);
    let before = std::fs::read(&path)?;
    std::fs::remove_file(data.path().join("catalog.db"))?;
    let reopened = Store::open_data_dir(data.path())?;
    assert_eq!(reopened.session_storage_policy(&root.id)?, Some(saved));
    assert_eq!(reopened.session_storage_policy(&child.id)?, Some(saved));
    reopened.catalog_rebuild()?;
    assert_eq!(reopened.storage_stats()?.chat_transaction_count, 0);
    drop(reopened);
    assert_eq!(std::fs::read(path)?, before);
    Ok(())
}

#[test]
fn housekeeping_prunes_only_each_roots_expired_rows_in_one_transaction() -> Result<()> {
    let data = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let store = Store::open_data_dir(data.path())?;
    let short = store.create_session_with_policy(
        "short",
        ContextStrategy::default(),
        workspace.path(),
        policy(1, 1),
    )?;
    let long = store.create_session_with_policy(
        "long",
        ContextStrategy::default(),
        workspace.path(),
        policy(24 * 30, 30),
    )?;
    let child =
        store.create_session_with_parent("child", ContextStrategy::default(), Some(&long.id))?;
    let old = (chrono::Utc::now() - chrono::Duration::days(7)).to_rfc3339();
    let serving = Connection::open(data.path().join("serving.db"))?;
    for (index, session) in [&short.id, &long.id, &child.id].into_iter().enumerate() {
        let run = format!("fixture-{index}");
        serving.execute(
            "INSERT INTO runs(id,session_id,status,data) VALUES(?1,?2,'completed',?3)",
            params![run, session, json!({"ended_at":old}).to_string()],
        )?;
        serving.execute("INSERT INTO events(seq,session_id,run_id,created_at,kind,data) VALUES(?1,?2,?3,?4,'run_finished','{}')", params![index as i64 + 1, session, run, old])?;
        serving.execute(
            "INSERT INTO telemetry(session_id,created_at,kind,data) VALUES(?1,?2,'fixture','{}')",
            params![session, old],
        )?;
        serving.execute("INSERT INTO approvals(id,session_id,run_id,call_id,tool,arguments,state,created_at,decided_at) VALUES(?1,?2,?1,'call','fixture','{}','allowed',?3,?3)", params![run, session, old])?;
        serving.execute("INSERT INTO inputs(id,session_id,kind,text,state,resolved_at,source_seq) VALUES(?1,?2,'followup','','consumed',?3,0)", params![run, session, old])?;
    }
    let expired_host = (chrono::Utc::now() - chrono::Duration::days(40)).to_rfc3339();
    for (session, created) in [
        (None, &old),
        (None, &expired_host),
        (Some("missing"), &expired_host),
    ] {
        serving.execute(
            "INSERT INTO telemetry(session_id,created_at,kind,data) VALUES(?1,?2,'host','{}')",
            params![session, created],
        )?;
    }
    let before = store.storage_stats()?;
    store.housekeeping()?;
    let after = store.storage_stats()?;
    assert_eq!(
        after.serving_transaction_count - before.serving_transaction_count,
        1
    );
    assert_eq!(after.chat_transaction_count, before.chat_transaction_count);
    for table in ["events", "runs", "approvals", "inputs", "telemetry"] {
        for (session, expected) in [(&short.id, 0), (&long.id, 1), (&child.id, 1)] {
            let count: i64 = serving.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE session_id=?1"),
                [session],
                |row| row.get(0),
            )?;
            assert_eq!(count, expected, "{table} for {session}");
        }
    }
    assert_eq!(
        serving.query_row(
            "SELECT COUNT(*) FROM telemetry WHERE kind='host'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    Ok(())
}

#[test]
fn idle_archive_uses_root_policy_and_archived_policy_reads_do_not_restore() -> Result<()> {
    let data = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let store = Store::open_with_options(
        data.path(),
        StoreOptions {
            archive_after_days: 1,
            ..Default::default()
        },
    )?;
    let mut enabled = policy(24, 30);
    enabled.store.archive_after_days = 1;
    let archive = store.create_session_with_policy(
        "archive",
        ContextStrategy::default(),
        workspace.path(),
        enabled,
    )?;
    let keep = store.create_session_with_policy(
        "keep",
        ContextStrategy::default(),
        workspace.path(),
        policy(24, 30),
    )?;
    let archive_path = store.chat_path(&archive.id)?.unwrap();
    let keep_path = store.chat_path(&keep.id)?.unwrap();
    let catalog = Connection::open(data.path().join("catalog.db"))?;
    catalog.execute(
        "UPDATE sessions SET last_activity=?1",
        [(chrono::Utc::now() - chrono::Duration::days(7)).to_rfc3339()],
    )?;
    assert_eq!(store.archive_idle_chats()?, 1);
    assert!(!archive_path.exists());
    assert!(keep_path.exists());
    assert_eq!(store.session_storage_policy(&archive.id)?, Some(enabled));
    assert!(!archive_path.exists());
    drop(catalog);
    drop(store);
    let reopened = Store::open_data_dir(data.path())?;
    assert_eq!(reopened.session_storage_policy(&archive.id)?, Some(enabled));
    assert!(!archive_path.exists());
    Ok(())
}

#[test]
fn malformed_canonical_storage_policy_is_rejected() -> Result<()> {
    let data = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let store = Store::open_data_dir(data.path())?;
    let root = store.create_session_with_policy(
        "bad",
        ContextStrategy::default(),
        workspace.path(),
        policy(24, 30),
    )?;
    let path = store.chat_path(&root.id)?.unwrap();
    drop(store);
    let conn = Connection::open(path)?;
    conn.execute(
        "UPDATE metadata SET value='{}' WHERE key=?1",
        [format!("root_storage:{}", root.id)],
    )?;
    drop(conn);
    assert!(Store::open_data_dir(data.path()).is_err());
    Ok(())
}

#[test]
fn automatic_cold_compression_uses_each_roots_thresholds() -> Result<()> {
    let data = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let store = Store::open_data_dir(data.path())?;
    let mut compress = policy(24, 30);
    compress.store.cold_min_messages = 1;
    compress.store.cold_min_bytes = 1;
    compress.store.cold_row_min_bytes = 1;
    compress.store.cold_tail_messages = 1;
    for (label, saved, expected) in [
        ("compress", compress, true),
        ("keep", policy(24, 30), false),
    ] {
        let root = store.create_session_with_policy(
            label,
            ContextStrategy::Classical,
            workspace.path(),
            saved,
        )?;
        let messages: Vec<_> = (0..12)
            .map(|index| Message {
                id: format!("{label}-{index}"),
                session_id: root.id.clone(),
                role: Role::User,
                origin: None,
                content: "Synthetic repeated message body for policy separation. ".repeat(30),
                tool_calls: vec![],
                tool_call_id: None,
                provider_state: None,
                is_error: false,
                created_at: chrono::Utc::now().to_rfc3339(),
            })
            .collect();
        store.append_batch(&messages)?;
        store.save_context_checkpoint(&ContextCheckpoint {
            id: format!("{label}-checkpoint"),
            session_id: root.id.clone(),
            through_message_id: messages.last().unwrap().id.clone(),
            model: "synthetic".into(),
            summary: "Synthetic policy fixture".into(),
            files_touched: vec![],
            created_at: chrono::Utc::now().to_rfc3339(),
            usage: None,
        })?;
        assert_eq!(store.compress_cold_messages(&root.id)? > 0, expected);
        assert_eq!(store.messages(&root.id)?, messages);
    }
    Ok(())
}
