use super::*;

#[test]
fn default_retention_prunes_finished_events_after_24_hours_and_telemetry_after_30_days()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let now = chrono::Utc::now();
    let old = (now - chrono::Duration::hours(48)).to_rfc3339();
    let recent = (now - chrono::Duration::hours(12)).to_rfc3339();
    let old_telemetry = (now - chrono::Duration::days(31)).to_rfc3339();
    let recent_telemetry = (now - chrono::Duration::days(29)).to_rfc3339();
    let session = {
        let store = Store::open_data_dir(directory.path())?;
        let session = store.create_session("retention")?;
        for date in [&old_telemetry, &recent_telemetry] {
            let mut record = bubblepaw_core::RequestUsage::new("synthetic", "model");
            record.session_id.clone_from(&session.id);
            record.created_at.clone_from(date);
            store.queue_request_usage(record)?;
        }
        store.flush_request_usage(&session.id)?;
        store.idle_checkpoint()?;
        session
    };
    let serving = directory.path().join("serving.db");
    let conn = Connection::open(&serving)?;
    for (id, status, date) in [
        ("active", "running", old.as_str()),
        ("old-finished", "completed", old.as_str()),
        ("recent-finished", "completed", recent.as_str()),
        ("expired-finished", "completed", old_telemetry.as_str()),
    ] {
        let run = json!({"id":id,"session_id":session.id,"model":"synthetic","status":status,"text":null,"error":null,"started_at":date,"ended_at":if status == "running" {None} else {Some(date)},"usage":null});
        conn.execute(
            "INSERT INTO runs(id,session_id,status,data) VALUES(?1,?2,?3,?4)",
            params![id, session.id, status, run.to_string()],
        )?;
    }
    // Active events can precede another session/run's finished events. Retention
    // must not erase the active row merely to make a removable journal prefix.
    for (seq, run, date) in [
        (1, "active", old.as_str()),
        (2, "old-finished", old.as_str()),
        (3, "recent-finished", recent.as_str()),
    ] {
        let event = json!({"sequence":seq,"durable":true,"session_id":session.id,"run_id":run,"created_at":date,"kind":{"type":"session_settled"}});
        conn.execute("INSERT INTO events(seq,session_id,run_id,created_at,kind,data) VALUES(?1,?2,?3,?4,'session_settled',?5)", params![seq, session.id, run, date, event.to_string()])?;
    }
    for (id, date) in [(1, old_telemetry.as_str()), (2, recent_telemetry.as_str())] {
        conn.execute("INSERT INTO telemetry(id,session_id,created_at,kind,data) VALUES(?1,?2,?3,'synthetic','{}')", params![id, session.id, date])?;
    }
    conn.execute("INSERT INTO approvals(id,session_id,run_id,call_id,tool,arguments,state,created_at) VALUES('active-approval',?1,'active','active-call','effect','{}','pending',?2)", params![session.id, old])?;
    conn.execute("INSERT INTO approvals(id,session_id,run_id,call_id,tool,arguments,state,created_at,decided_at) VALUES('finished-approval',?1,'old-finished','finished-call','effect','{}','approved',?2,?2)", params![session.id, old])?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    drop(conn);
    let store = Store::open_with_options(directory.path(), StoreOptions::default())?;
    let conn = Connection::open(&serving)?;
    let kept_events = conn
        .prepare("SELECT seq FROM events ORDER BY seq")?
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    assert_eq!(kept_events, vec![1, 3]);
    let kept_runs = conn
        .prepare("SELECT id FROM runs ORDER BY id")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    assert_eq!(kept_runs, vec!["active", "old-finished", "recent-finished"]);
    assert_eq!(rows(&serving, "approvals")?, 1);
    assert_eq!(rows(&serving, "telemetry")?, 1);
    assert_eq!(
        conn.query_row("SELECT id FROM telemetry", [], |r| r.get::<_, i64>(0))?,
        2
    );
    assert!(!has_table(&chat_path(&store, &session.id)?, "telemetry")?);
    store.housekeeping()?;
    assert_eq!(rows(&serving, "events")?, 2);
    assert_eq!(
        store.request_usage()?.len(),
        2,
        "authoritative usage has no telemetry retention cutoff"
    );
    assert!(!has_table(&serving, "usage")?);
    Ok(())
}
