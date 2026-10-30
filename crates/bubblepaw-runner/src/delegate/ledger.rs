//! `delegations.db`: one attempt row and its call rows per transaction.
use super::metrics::{AttemptMetrics, CallRow};
use super::pricing::TokenClasses;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::Path;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS delegation_attempts (
    id TEXT PRIMARY KEY,
    delegation_id TEXT NOT NULL,
    parent_id TEXT,
    started_at TEXT NOT NULL,
    ended_at TEXT NOT NULL,
    assignment_hash TEXT NOT NULL,
    category TEXT NOT NULL,
    tags TEXT NOT NULL,
    workspace TEXT NOT NULL,
    repo TEXT,
    assignment_tokens INTEGER NOT NULL,
    check_count INTEGER NOT NULL,
    route_class TEXT,
    model TEXT NOT NULL,
    route_reason TEXT NOT NULL,
    period TEXT NOT NULL,
    rework_index INTEGER NOT NULL,
    session_id TEXT NOT NULL,
    run_status TEXT NOT NULL,
    report_status TEXT,
    final_status TEXT NOT NULL,
    accepted INTEGER NOT NULL,
    checks TEXT NOT NULL,
    false_greens INTEGER NOT NULL,
    uncached_input INTEGER NOT NULL,
    cache_read INTEGER NOT NULL,
    cache_write INTEGER NOT NULL,
    output INTEGER NOT NULL,
    reasoning INTEGER,
    cost_usd REAL,
    price_table TEXT NOT NULL,
    wall_ms INTEGER NOT NULL,
    model_ms INTEGER,
    tool_ms INTEGER,
    check_ms INTEGER NOT NULL,
    cache_hit_ratio REAL,
    median_tps REAL,
    p90_tps REAL,
    call_count INTEGER NOT NULL,
    tool_errors INTEGER NOT NULL,
    malformed_calls INTEGER NOT NULL,
    repeated_calls INTEGER NOT NULL,
    done_without_check INTEGER NOT NULL,
    tokens_to_first_pass INTEGER,
    ms_to_first_pass INTEGER,
    lcm_gathers INTEGER,
    lcm_applies INTEGER,
    lcm_rejected INTEGER,
    files_changed INTEGER,
    lines_added INTEGER,
    lines_removed INTEGER,
    provider_errors INTEGER NOT NULL,
    retries INTEGER,
    fallbacks INTEGER,
    rate_limits INTEGER,
    timeouts INTEGER,
    unknown_effects INTEGER NOT NULL,
    no_final_report INTEGER NOT NULL,
    metrics TEXT NOT NULL,
    undeclared_busts INTEGER NOT NULL DEFAULT 0,
    unexplained_miss_tokens INTEGER
);
CREATE INDEX IF NOT EXISTS delegation_attempts_delegation
    ON delegation_attempts(delegation_id);
CREATE TABLE IF NOT EXISTS delegation_calls (
    attempt_id TEXT NOT NULL REFERENCES delegation_attempts(id),
    seq INTEGER NOT NULL,
    model TEXT NOT NULL,
    provider_route TEXT,
    account TEXT,
    fallback_reason TEXT,
    uncached_input INTEGER,
    cache_read INTEGER,
    cache_write INTEGER,
    output INTEGER,
    reasoning INTEGER,
    ttft_ms INTEGER,
    generation_ms INTEGER,
    tokens_per_s REAL,
    outcome TEXT NOT NULL,
    expected_cacheable INTEGER,
    unexplained_miss INTEGER,
    prefix_break TEXT,
    wire_bytes INTEGER,
    PRIMARY KEY (attempt_id, seq)
) WITHOUT ROWID;
"#;

/// Columns added after the first release, as (table, column, definition). Ledgers
/// created earlier gain each missing one in place; no existing row is rewritten.
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
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

/// The ledger's only numbered step. A first-release ledger carries no version and
/// lacks the columns above; step 1 adds them and no later open inspects a table.
const VERSION: i64 = 1;

/// Run once per ledger: step 1 adds each missing column of `ADDED_COLUMNS`. A
/// ledger the previous per-open code already upgraded only gains the version, so
/// each column is checked once and later opens do no schema inspection.
fn migrate(connection: &mut Connection) -> Result<()> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= VERSION {
        return Ok(());
    }
    let tx = connection.transaction()?;
    for (table, column, definition) in ADDED_COLUMNS {
        let present: Vec<String> = tx
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        if !present.iter().any(|name| name.as_str() == *column) {
            tx.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {definition}"
            ))?;
        }
    }
    tx.execute_batch(&format!("PRAGMA user_version={VERSION};"))?;
    tx.commit()?;
    Ok(())
}

/// Everything recorded about one attempt, written in one transaction.
#[derive(Clone, Debug, Serialize)]
pub struct AttemptRecord {
    pub id: String,
    pub delegation_id: String,
    pub parent_id: Option<String>,
    pub started_at: String,
    pub ended_at: String,
    pub assignment_hash: String,
    pub category: String,
    pub tags: Vec<String>,
    pub workspace: String,
    pub repo: Option<String>,
    pub assignment_tokens: u64,
    pub check_count: usize,
    pub route_class: Option<String>,
    pub model: String,
    pub route_reason: String,
    pub period: String,
    pub rework_index: u32,
    pub session_id: String,
    pub run_status: String,
    pub report_status: Option<String>,
    /// accepted, rework, failed or unchecked.
    pub final_status: String,
    pub accepted: bool,
    pub checks: Value,
    pub false_greens: usize,
    pub tokens: TokenClasses,
    pub cost_usd: Option<f64>,
    pub price_table: String,
    pub wall_ms: u64,
    pub check_ms: u64,
    pub metrics: AttemptMetrics,
    pub calls: Vec<CallRow>,
}

pub struct Ledger {
    connection: Connection,
}

fn int(value: Option<u64>) -> Option<i64> {
    value.map(|v| v as i64)
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut connection = Connection::open(path)
            .with_context(|| format!("open delegation ledger {}", path.display()))?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(SCHEMA)?;
        migrate(&mut connection)?;
        Ok(Self { connection })
    }

    pub fn attempt_exists(&self, id: &str) -> Result<bool> {
        Ok(self
            .connection
            .query_row(
                "SELECT 1 FROM delegation_attempts WHERE id = ?1",
                [id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Insert the attempt and its calls in a single transaction.
    pub fn record(&mut self, a: &AttemptRecord) -> Result<()> {
        let m = &a.metrics;
        let tx = self.connection.transaction()?;
        tx.execute(
            "INSERT INTO delegation_attempts (id, delegation_id, parent_id, started_at, ended_at, assignment_hash, category, tags, workspace, repo, assignment_tokens, check_count, route_class, model, route_reason, period, rework_index, session_id, run_status, report_status, final_status, accepted, checks, false_greens, uncached_input, cache_read, cache_write, output, reasoning, cost_usd, price_table, wall_ms, model_ms, tool_ms, check_ms, cache_hit_ratio, median_tps, p90_tps, call_count, tool_errors, malformed_calls, repeated_calls, done_without_check, tokens_to_first_pass, ms_to_first_pass, lcm_gathers, lcm_applies, lcm_rejected, files_changed, lines_added, lines_removed, provider_errors, retries, fallbacks, rate_limits, timeouts, unknown_effects, no_final_report, metrics, undeclared_busts, unexplained_miss_tokens) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30,?31,?32,?33,?34,?35,?36,?37,?38,?39,?40,?41,?42,?43,?44,?45,?46,?47,?48,?49,?50,?51,?52,?53,?54,?55,?56,?57,?58,?59,?60,?61)",
            rusqlite::params_from_iter([
                &a.id as &dyn rusqlite::ToSql,
                &a.delegation_id,
                &a.parent_id,
                &a.started_at,
                &a.ended_at,
                &a.assignment_hash,
                &a.category,
                &serde_json::to_string(&a.tags)?,
                &a.workspace,
                &a.repo,
                &(a.assignment_tokens as i64),
                &(a.check_count as i64),
                &a.route_class,
                &a.model,
                &a.route_reason,
                &a.period,
                &a.rework_index,
                &a.session_id,
                &a.run_status,
                &a.report_status,
                &a.final_status,
                &a.accepted,
                &a.checks.to_string(),
                &(a.false_greens as i64),
                &(a.tokens.uncached_input as i64),
                &(a.tokens.cache_read as i64),
                &(a.tokens.cache_write as i64),
                &(a.tokens.output as i64),
                &int(a.tokens.reasoning),
                &a.cost_usd,
                &a.price_table,
                &(a.wall_ms as i64),
                &int(m.model_ms),
                &int(m.tool_ms),
                &(a.check_ms as i64),
                &m.cache_hit_ratio,
                &m.median_tps,
                &m.p90_tps,
                &(m.call_count as i64),
                &(m.tool_errors as i64),
                &(m.malformed_calls as i64),
                &(m.repeated_calls as i64),
                &m.done_without_check,
                &int(m.tokens_to_first_pass),
                &int(m.ms_to_first_pass),
                &int(m.lcm_gathers),
                &int(m.lcm_applies),
                &int(m.lcm_rejected),
                &int(m.files_changed),
                &int(m.lines_added),
                &int(m.lines_removed),
                &(m.reliability.provider_errors as i64),
                &int(m.reliability.retries),
                &int(m.reliability.fallbacks),
                &int(m.reliability.rate_limits),
                &int(m.reliability.timeouts),
                &(m.reliability.unknown_effects as i64),
                &m.reliability.no_final_report,
                &serde_json::to_string(&json!({
                    "tool_calls_by_name": m.tool_calls_by_name,
                    "tps": m.tps,
                    "dropped_records": m.dropped_records,
                }))?,
                &(m.undeclared_busts as i64),
                &int(m.unexplained_miss_tokens),
            ]),
        )?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO delegation_calls (attempt_id, seq, model, provider_route, account, fallback_reason, uncached_input, cache_read, cache_write, output, reasoning, ttft_ms, generation_ms, tokens_per_s, outcome, expected_cacheable, unexplained_miss, prefix_break, wire_bytes) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
            )?;
            for call in &a.calls {
                let t = call.tokens.as_ref();
                insert.execute(params![
                    a.id,
                    call.seq as i64,
                    call.model,
                    call.provider_route,
                    call.account,
                    call.fallback_reason,
                    t.map(|t| t.uncached_input as i64),
                    t.map(|t| t.cache_read as i64),
                    t.map(|t| t.cache_write as i64),
                    t.map(|t| t.output as i64),
                    t.and_then(|t| int(t.reasoning)),
                    int(call.ttft_ms),
                    int(call.generation_ms),
                    call.tokens_per_s,
                    call.outcome,
                    int(call.expected_cacheable_tokens),
                    int(call.unexplained_miss_tokens),
                    call.prefix_break.clone(),
                    int(call.wire_bytes),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Cost per accepted outcome, grouped by category, class, model and route reason.
    /// A delegation is one `delegate` invocation; its cost includes every rework attempt.
    pub fn report(&self) -> Result<Value> {
        let mut statement = self.connection.prepare(
            r#"
            WITH d AS (
                SELECT delegation_id,
                       MIN(category) AS category,
                       MIN(COALESCE(route_class, 'named')) AS class,
                       MIN(model) AS model,
                       MIN(route_reason) AS reason,
                       MAX(accepted) AS accepted,
                       COUNT(*) AS attempts,
                       SUM(cost_usd) AS cost,
                       SUM(cost_usd IS NULL) AS unpriced,
                       SUM(uncached_input + cache_read + cache_write + output) AS tokens,
                       SUM(wall_ms) AS wall_ms,
                       SUM(false_greens) AS false_greens,
                       SUM(provider_errors) AS provider_errors,
                       SUM(COALESCE(retries, 0)) AS retries,
                       SUM(COALESCE(fallbacks, 0)) AS fallbacks,
                       SUM(rate_limits) AS rate_limits,
                       SUM(timeouts) AS timeouts,
                       SUM(unknown_effects) AS unknown_effects,
                       SUM(no_final_report) AS no_final_report,
                       AVG(cache_hit_ratio) AS cache_hit_ratio,
                       AVG(median_tps) AS median_tps
                FROM delegation_attempts GROUP BY delegation_id
            )
            SELECT category, class, model, reason,
                   COUNT(*), SUM(accepted), SUM(attempts), SUM(cost), SUM(unpriced),
                   SUM(tokens), SUM(wall_ms), SUM(false_greens),
                   SUM(provider_errors), SUM(retries), SUM(fallbacks), SUM(rate_limits),
                   SUM(timeouts), SUM(unknown_effects), SUM(no_final_report),
                   AVG(cache_hit_ratio), AVG(median_tps)
            FROM d GROUP BY category, class, model, reason
            ORDER BY category, class, model, reason
            "#,
        )?;
        let rows = statement.query_map([], |row| {
            let delegations: i64 = row.get(4)?;
            let accepted: i64 = row.get(5)?;
            let cost: Option<f64> = row.get(7)?;
            let unpriced: i64 = row.get(8)?;
            let tokens: Option<i64> = row.get(9)?;
            Ok(json!({
                "category": row.get::<_, String>(0)?,
                "class": row.get::<_, String>(1)?,
                "model": row.get::<_, String>(2)?,
                "reason": row.get::<_, String>(3)?,
                "delegations": delegations,
                "accepted": accepted,
                "attempts": row.get::<_, i64>(6)?,
                "cost_usd": cost,
                "unpriced_attempts": unpriced,
                "cost_per_accepted_usd": (accepted > 0 && unpriced == 0)
                    .then(|| cost.unwrap_or(0.0) / accepted as f64),
                "tokens_per_accepted": (accepted > 0)
                    .then(|| tokens.unwrap_or(0) as f64 / accepted as f64),
                "wall_ms": row.get::<_, Option<i64>>(10)?,
                "false_greens": row.get::<_, Option<i64>>(11)?,
                "reliability": {
                    "provider_errors": row.get::<_, Option<i64>>(12)?,
                    "retries": row.get::<_, Option<i64>>(13)?,
                    "fallbacks": row.get::<_, Option<i64>>(14)?,
                    "rate_limits": row.get::<_, Option<i64>>(15)?,
                    "timeouts": row.get::<_, Option<i64>>(16)?,
                    "unknown_effects": row.get::<_, Option<i64>>(17)?,
                    "no_final_report": row.get::<_, Option<i64>>(18)?,
                },
                "cache_hit_ratio": row.get::<_, Option<f64>>(19)?,
                "median_tps": row.get::<_, Option<f64>>(20)?,
            }))
        })?;
        Ok(json!({ "groups": rows.collect::<rusqlite::Result<Vec<_>>>()? }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns(connection: &Connection, table: &str) -> Result<Vec<String>> {
        Ok(connection
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<String>>>()?)
    }

    #[test]
    fn old_ledger_gains_the_cache_columns_and_stores_them() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("delegations.db");
        // The first-release DDL: the cache columns removed from the current schema.
        let old = SCHEMA
            .replace(
                "    metrics TEXT NOT NULL,\n    undeclared_busts INTEGER NOT NULL DEFAULT 0,\n    unexplained_miss_tokens INTEGER\n",
                "    metrics TEXT NOT NULL\n",
            )
            .replace(
                "    outcome TEXT NOT NULL,\n    expected_cacheable INTEGER,\n    unexplained_miss INTEGER,\n    prefix_break TEXT,\n    wire_bytes INTEGER,\n",
                "    outcome TEXT NOT NULL,\n",
            );
        assert!(!old.contains("wire_bytes") && !old.contains("undeclared_busts"));
        Connection::open(&path)?.execute_batch(&old)?;

        let mut ledger = Ledger::open(&path)?;
        let attempts = columns(&ledger.connection, "delegation_attempts")?;
        let calls = columns(&ledger.connection, "delegation_calls")?;
        assert!(attempts.iter().any(|c| c == "undeclared_busts"));
        assert!(attempts.iter().any(|c| c == "unexplained_miss_tokens"));
        for column in [
            "expected_cacheable",
            "unexplained_miss",
            "prefix_break",
            "wire_bytes",
        ] {
            assert!(calls.iter().any(|c| c == column), "missing {column}");
        }

        let attempt = AttemptRecord {
            id: "a1".into(),
            delegation_id: "d1".into(),
            parent_id: None,
            started_at: "2026-10-19T00:00:00Z".into(),
            ended_at: "2026-10-19T00:00:01Z".into(),
            assignment_hash: "h".into(),
            category: "test".into(),
            tags: vec![],
            workspace: "w".into(),
            repo: None,
            assignment_tokens: 1,
            check_count: 1,
            route_class: None,
            model: "m".into(),
            route_reason: "r".into(),
            period: "p".into(),
            rework_index: 0,
            session_id: "s".into(),
            run_status: "done".into(),
            report_status: Some("done".into()),
            final_status: "accepted".into(),
            accepted: true,
            checks: json!([]),
            false_greens: 0,
            tokens: TokenClasses {
                uncached_input: 100,
                cache_read: 300,
                cache_write: 0,
                output: 500,
                reasoning: None,
            },
            cost_usd: None,
            price_table: "t".into(),
            wall_ms: 1,
            check_ms: 0,
            metrics: AttemptMetrics {
                undeclared_busts: 2,
                unexplained_miss_tokens: Some(14),
                ..Default::default()
            },
            calls: vec![CallRow {
                seq: 0,
                model: "m".into(),
                provider_route: Some("p/m".into()),
                account: None,
                fallback_reason: None,
                tokens: None,
                ttft_ms: None,
                generation_ms: None,
                tokens_per_s: None,
                outcome: "ok".into(),
                expected_cacheable_tokens: Some(64),
                unexplained_miss_tokens: Some(14),
                prefix_break: Some("undeclared".into()),
                wire_bytes: Some(4096),
                at: None,
            }],
        };
        ledger.record(&attempt)?;

        let (busts, unexplained): (i64, Option<i64>) = ledger.connection.query_row(
            "SELECT undeclared_busts, unexplained_miss_tokens FROM delegation_attempts WHERE id = ?1",
            ["a1"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!((busts, unexplained), (2, Some(14)));
        let call: (Option<i64>, Option<i64>, Option<String>, Option<i64>) =
            ledger.connection.query_row(
                "SELECT expected_cacheable, unexplained_miss, prefix_break, wire_bytes FROM delegation_calls WHERE attempt_id = ?1",
                ["a1"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        assert_eq!(
            call,
            (
                Some(64),
                Some(14),
                Some("undeclared".to_owned()),
                Some(4096)
            )
        );
        drop(ledger);

        // Reopening an migrated ledger adds nothing and keeps the stored row.
        let reopened = Ledger::open(&path)?;
        assert!(reopened.attempt_exists("a1")?);
        Ok(())
    }
}
