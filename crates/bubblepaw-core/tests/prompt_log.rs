//! The serving-side prompt-change log: one row per request, each distinct
//! frozen/cold tier once by hash, and both pruned with telemetry.
use anyhow::Result;
use bubblepaw_core::{
    ContextState, Message, ModelRequest, ReasoningEffort, Role, Store, StoreOptions, Usage,
    fingerprint,
};

fn request(system: &str, contents: &[&str]) -> ModelRequest {
    ModelRequest {
        model: "route/main".into(),
        system_prompt: system.into(),
        messages: contents
            .iter()
            .map(|content| Message {
                id: String::new(),
                session_id: "s".into(),
                role: Role::User,
                origin: None,
                content: (*content).into(),
                tool_calls: Vec::new(),
                tool_call_id: None,
                provider_state: None,
                is_error: false,
                created_at: String::new(),
            })
            .collect(),
        ..ModelRequest::default()
    }
}
fn usage(input: u64, cached: u64) -> Usage {
    Usage {
        input_tokens: input,
        cache_read_tokens: cached,
        ..Usage::default()
    }
}
fn tier_rows(dir: &std::path::Path) -> Result<i64> {
    let serving = rusqlite::Connection::open(dir.join("serving.db"))?;
    Ok(serving.query_row("SELECT COUNT(*) FROM prompt_tiers", [], |row| row.get(0))?)
}

#[test]
fn each_request_is_one_row_and_each_tier_is_stored_once() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let context = ContextState::default();
    context.attach_request_log(&store);
    let mut history = Vec::new();
    for (turn, content) in ["one", "two", "three", "four"].into_iter().enumerate() {
        history.push(content);
        // Cold `[2]` is set from the third request on: one cold change.
        let cold: &[usize] = if turn >= 2 { &[0] } else { &[] };
        context.build(request("system", &history), "s", "root", cold);
        context.finish_request("s", "root", "go", "route/main", Some(&usage(100, 50)), None);
    }
    let rows = store.model_requests("s")?;
    assert_eq!(rows.len(), 4, "one row per request");
    assert_eq!(
        rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    assert_eq!(
        rows.iter()
            .map(|row| row.classification.as_str())
            .collect::<Vec<_>>(),
        ["first", "append", "cold_change", "append"]
    );
    assert!(rows.iter().all(|row| row.cached_tokens == Some(50)));
    assert!(rows.iter().all(|row| row.uncached_tokens == Some(50)));
    assert!(rows.iter().all(|row| row.provider == "go"));
    // The frozen tier never changed: stored once, with the exact hashed bytes.
    let frozen = store.prompt_tier(&rows[0].frozen.sha256)?.unwrap();
    assert_eq!(fingerprint(frozen.as_bytes()), rows[0].frozen.sha256);
    assert_eq!(frozen.len(), rows[0].frozen.bytes);
    let cold = store.prompt_tier(&rows[3].cold.sha256)?.unwrap();
    assert_eq!(fingerprint(cold.as_bytes()), rows[3].cold.sha256);
    store.flush_serving()?;
    // One frozen tier and two cold tiers (empty, then one message) for four rows.
    assert_eq!(tier_rows(dir.path())?, 3);
    Ok(())
}

#[test]
fn a_row_waits_for_usage_but_is_written_when_superseded() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let context = ContextState::default();
    context.attach_request_log(&store);
    context.build(request("system", &["one"]), "s", "decider", &[]);
    assert!(store.model_requests("s")?.is_empty());
    context.build(request("system", &["one", "two"]), "s", "decider", &[]);
    let rows = store.model_requests("s")?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].cached_tokens, None);
    context.release_session("s");
    assert_eq!(store.model_requests("s")?.len(), 2);
    Ok(())
}

#[test]
fn retention_deletes_rows_and_their_tiers() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let options = StoreOptions {
        telemetry_retention_days: 0,
        ..StoreOptions::default()
    };
    let store = Store::open_with_options(dir.path(), options)?;
    let context = ContextState::default();
    context.attach_request_log(&store);
    context.build(request("system", &["one"]), "s", "root", &[]);
    context.finish_request("s", "root", "go", "route/main", Some(&usage(10, 0)), None);
    let rows = store.model_requests("s")?;
    assert_eq!(rows.len(), 1);
    let sha = rows[0].frozen.sha256.clone();
    assert!(store.prompt_tier(&sha)?.is_some());
    store.housekeeping()?;
    assert!(store.model_requests("s")?.is_empty());
    assert_eq!(store.prompt_tier(&sha)?, None);
    assert_eq!(tier_rows(dir.path())?, 0);
    Ok(())
}

#[test]
fn a_store_without_a_serving_view_logs_nothing() -> Result<()> {
    let store = Store::in_memory()?;
    let context = ContextState::default();
    context.attach_request_log(&store);
    context.build(request("system", &["one"]), "s", "root", &[]);
    context.finish_request("s", "root", "go", "route/main", Some(&usage(10, 0)), None);
    assert!(store.model_requests("s")?.is_empty());
    Ok(())
}

#[test]
fn rows_carry_writes_output_reasoning_breakpoints_and_lifetime() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let context = ContextState::default();
    context.attach_request_log(&store);
    let mut first = request("system", &["one"]);
    first.reasoning_effort = Some(ReasoningEffort::High);
    context.build(first, "s", "root", &[]);
    let answered = Usage {
        cache_creation_tokens: 0,
        output_tokens: 7,
        reasoning_tokens: Some(3),
        ..usage(100, 0)
    };
    let marks = vec!["system.0".to_owned(), "messages.0.0".to_owned()];
    context.finish_request(
        "s",
        "root",
        "anthropic",
        "claude-x",
        Some(&answered),
        Some(marks.clone()),
    );
    // A provider without a known cache reporting zero writes: not reported.
    context.build(request("system", &["one", "two"]), "s", "root", &[]);
    context.finish_request("s", "root", "go", "route/main", Some(&usage(100, 50)), None);
    // A nonzero write is always kept.
    context.build(
        request("system", &["one", "two", "three"]),
        "s",
        "root",
        &[],
    );
    let written = Usage {
        cache_creation_tokens: 40,
        ..usage(100, 50)
    };
    context.finish_request("s", "root", "go", "route/main", Some(&written), None);
    let rows = store.model_requests("s")?;
    assert_eq!(rows.len(), 3);
    let anthropic = &rows[0];
    assert_eq!(
        anthropic.cache_write_tokens,
        Some(0),
        "Anthropic reports writes"
    );
    assert_eq!(anthropic.output_tokens, Some(7));
    assert_eq!(anthropic.reasoning_tokens, Some(3));
    assert_eq!(anthropic.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(anthropic.breakpoints, Some(marks));
    assert_eq!(anthropic.cache_lifetime_ms, Some(300_000));
    assert_eq!(anthropic.likely_expired, None, "a first request has no gap");
    let unreported = &rows[1];
    assert_eq!(unreported.cache_write_tokens, None);
    assert_eq!(unreported.output_tokens, Some(0));
    assert_eq!(unreported.reasoning_tokens, None);
    assert_eq!(unreported.reasoning_effort, None);
    assert_eq!(unreported.breakpoints, None);
    assert_eq!(unreported.cache_lifetime_ms, None);
    assert_eq!(unreported.likely_expired, None);
    assert_eq!(rows[2].cache_write_tokens, Some(40));
    // Stored as compact JSON.
    store.flush_serving()?;
    let serving = rusqlite::Connection::open(dir.path().join("serving.db"))?;
    let stored: String = serving.query_row(
        "SELECT breakpoints FROM model_requests WHERE seq=1",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(stored, r#"["system.0","messages.0.0"]"#);
    Ok(())
}

#[test]
fn an_old_serving_store_gains_the_new_columns_and_keeps_its_rows() -> Result<()> {
    let dir = tempfile::tempdir()?;
    drop(Store::open_data_dir(dir.path())?);
    {
        // The first release's table, with one row.
        let serving = rusqlite::Connection::open(dir.path().join("serving.db"))?;
        serving.execute_batch("PRAGMA user_version=1;
DROP TABLE model_requests;
CREATE TABLE model_requests(id INTEGER PRIMARY KEY,session_id TEXT NOT NULL,lineage TEXT NOT NULL,seq INTEGER NOT NULL,created_at TEXT NOT NULL,frozen_sha256 TEXT NOT NULL,frozen_bytes INTEGER NOT NULL,cold_sha256 TEXT NOT NULL,cold_bytes INTEGER NOT NULL,cold_count INTEGER NOT NULL,history_sha256 TEXT NOT NULL,history_bytes INTEGER NOT NULL,history_count INTEGER NOT NULL,classification TEXT NOT NULL,declared INTEGER NOT NULL,declaration_reason TEXT,provider TEXT NOT NULL,model TEXT NOT NULL,cached_tokens INTEGER,uncached_tokens INTEGER,expected_cached_tokens INTEGER,since_previous_ms INTEGER,warned INTEGER NOT NULL);
INSERT INTO model_requests(session_id,lineage,seq,created_at,frozen_sha256,frozen_bytes,cold_sha256,cold_bytes,cold_count,history_sha256,history_bytes,history_count,classification,declared,provider,model,cached_tokens,warned) VALUES('s','root',1,'2099-01-01T00:00:00Z','f',1,'c',0,0,'h',1,1,'first',0,'go','route/main',5,0);")?;
    }
    let store = Store::open_data_dir(dir.path())?;
    let context = ContextState::default();
    context.attach_request_log(&store);
    context.build(request("system", &["one"]), "s", "root", &[]);
    let answered = Usage {
        output_tokens: 2,
        ..usage(10, 0)
    };
    context.finish_request("s", "root", "go", "route/main", Some(&answered), None);
    let rows = store.model_requests("s")?;
    assert_eq!(rows.len(), 2, "the old row survives the upgrade");
    assert_eq!(rows[0].cached_tokens, Some(5));
    assert_eq!(rows[0].output_tokens, None);
    assert_eq!(rows[0].breakpoints, None);
    assert_eq!(rows[1].output_tokens, Some(2));
    Ok(())
}
