//! The delegation ledger keeps one numbered step. A ledger of the first release,
//! which carried no version, still opens with its rows.
use anyhow::Result;
use bubblepaw_runner::delegate::ledger::Ledger;
use rusqlite::Connection;
use std::path::Path;

const LEDGER_V0: &str = include_str!("fixtures/ledger-v0-schema.sql");

/// The columns the release added on every open, as (table, column, definition).
const ADDED_COLUMNS: [(&str, &str, &str); 6] = [
    (
        "delegation_attempts",
        "undeclared_busts",
        "INTEGER NOT NULL DEFAULT 0",
    ),
    ("delegation_attempts", "unexplained_miss_tokens", "INTEGER"),
    ("delegation_calls", "expected_cacheable", "INTEGER"),
    ("delegation_calls", "unexplained_miss", "INTEGER"),
    ("delegation_calls", "prefix_break", "TEXT"),
    ("delegation_calls", "wire_bytes", "INTEGER"),
];

fn version(path: &Path) -> Result<i64> {
    let conn = Connection::open(path)?;
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let sql = format!("PRAGMA table_info({table})");
    Ok(conn
        .prepare(&sql)?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<String>>>()?)
}

#[test]
fn old_style_ledger_opens() -> Result<()> {
    for applied in [false, true] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("delegations.db");
        let conn = Connection::open(&path)?;
        conn.execute_batch(LEDGER_V0)?;
        conn.execute(
            "INSERT INTO delegation_attempts(id,delegation_id,started_at,ended_at,assignment_hash,category,tags,workspace,assignment_tokens,check_count,model,route_reason,period,rework_index,session_id,run_status,final_status,accepted,checks,false_greens,uncached_input,cache_read,cache_write,output,price_table,wall_ms,check_ms,call_count,tool_errors,malformed_calls,repeated_calls,done_without_check,provider_errors,unknown_effects,no_final_report,metrics) VALUES('old-attempt','old-attempt','t','t','h','c','[]','w',1,0,'m','r','p',0,'s','completed','accepted',1,'[]',0,1,0,0,1,'t',1,0,1,0,0,0,0,0,0,0,'{}')",
            [],
        )?;
        if applied {
            // The release added each missing column on every open, without a bump.
            for (table, column, definition) in ADDED_COLUMNS {
                let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {definition}");
                conn.execute_batch(&sql)?;
            }
        }
        assert_eq!(version(&path)?, 0);
        drop(conn);

        let ledger = Ledger::open(&path)?;
        assert!(ledger.attempt_exists("old-attempt")?);
        let conn = Connection::open(&path)?;
        assert_eq!(version(&path)?, 1, "ledger step 1");
        for (table, column, _) in ADDED_COLUMNS {
            let names = columns(&conn, table)?;
            assert!(names.iter().any(|name| name == column), "missing {column}");
        }
        drop(conn);
        drop(ledger);

        // A later open inspects no table and keeps the row.
        let reopened = Ledger::open(&path)?;
        assert_eq!(version(&path)?, 1);
        assert!(reopened.attempt_exists("old-attempt")?);
    }
    Ok(())
}
