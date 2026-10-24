//! Serving-side prompt-change log. One row per model request and each distinct
//! frozen or cold tier once by hash. The rows join the buffered serving
//! transaction (no COMMIT of their own) and are pruned with telemetry.
use super::*;
use crate::{RequestLogRow, TierFingerprint};

/// Serving-only setup: set the pragmas and create the request log (`model_requests`,
/// `prompt_tiers`).
const SERVING_SETUP: &str = "PRAGMA synchronous=NORMAL; PRAGMA cache_spill=OFF;
CREATE TABLE IF NOT EXISTS model_requests(id INTEGER PRIMARY KEY,session_id TEXT NOT NULL,lineage TEXT NOT NULL,seq INTEGER NOT NULL,created_at TEXT NOT NULL,frozen_sha256 TEXT NOT NULL,frozen_bytes INTEGER NOT NULL,cold_sha256 TEXT NOT NULL,cold_bytes INTEGER NOT NULL,cold_count INTEGER NOT NULL,history_sha256 TEXT NOT NULL,history_bytes INTEGER NOT NULL,history_count INTEGER NOT NULL,classification TEXT NOT NULL,declared INTEGER NOT NULL,declaration_reason TEXT,provider TEXT NOT NULL,model TEXT NOT NULL,cached_tokens INTEGER,uncached_tokens INTEGER,expected_cached_tokens INTEGER,since_previous_ms INTEGER,warned INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS prompt_tiers(sha256 TEXT PRIMARY KEY,content TEXT NOT NULL) WITHOUT ROWID;";

/// Nullable columns added after the first request-log release, by serving step 2.
const ADDED_COLUMNS: [(&str, &str); 7] = [
    ("cache_write_tokens", "INTEGER"),
    ("output_tokens", "INTEGER"),
    ("reasoning_tokens", "INTEGER"),
    ("reasoning_effort", "TEXT"),
    ("breakpoints", "TEXT"),
    ("cache_lifetime_ms", "INTEGER"),
    ("likely_expired", "INTEGER"),
];

/// Run at every serving open. The added columns arrive with step 2.
pub(in crate::store) fn setup(conn: &Connection) -> Result<()> {
    conn.execute_batch(SERVING_SETUP)?;
    Ok(())
}

/// Serving step 2. Drops the legacy `usage` table, which no release table writes,
/// and gives an older `model_requests` the added columns in place (null in its
/// existing rows). A log the previous per-open code already upgraded only gains
/// the version, so each column is checked once.
pub(in crate::store) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("DROP TABLE IF EXISTS usage;")?;
    let existing = conn
        .prepare("SELECT name FROM pragma_table_info('model_requests')")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (name, kind) in ADDED_COLUMNS {
        if !existing.iter().any(|column| column == name) {
            conn.execute_batch(&format!(
                "ALTER TABLE model_requests ADD COLUMN {name} {kind}"
            ))?;
        }
    }
    Ok(())
}

/// The context state's handle; it never keeps the store open.
pub(crate) struct WeakStore(Weak<Inner>);
impl WeakStore {
    pub(crate) fn upgrade(&self) -> Option<Store> {
        self.0.upgrade().map(Store)
    }
}

impl Store {
    pub(crate) fn downgrade(&self) -> WeakStore {
        WeakStore(Arc::downgrade(&self.0))
    }
    /// The serving view: a directory store's own, or a chat's host. A plain
    /// single-file or in-memory store has none, so nothing is logged.
    fn prompt_log_view(&self) -> Option<Store> {
        if self.is_layout() {
            Some(self.clone())
        } else {
            self.host()
        }
    }
    /// One buffered serving write per request: its row and any changed tiers.
    pub(crate) fn log_model_request(
        &self,
        row: &RequestLogRow,
        tiers: &[(String, String)],
    ) -> Result<()> {
        let Some(view) = self.prompt_log_view() else {
            return Ok(());
        };
        // SQLite integers are i64; sizes and counts here never approach i64::MAX.
        let number = |value: Option<u64>| value.map(|value| value as i64);
        let size = |value: usize| value as i64;
        let breakpoints = row
            .breakpoints
            .as_ref()
            .map(|marks| serde_json::to_string(marks).expect("strings serialize"));
        view.write_serving(|conn| {
            let mut tier = conn.prepare_cached(
                "INSERT OR IGNORE INTO prompt_tiers(sha256,content) VALUES(?1,?2)",
            )?;
            for (sha256, content) in tiers {
                tier.execute(params![sha256, content])?;
            }
            conn.prepare_cached("INSERT INTO model_requests(session_id,lineage,seq,created_at,frozen_sha256,frozen_bytes,cold_sha256,cold_bytes,cold_count,history_sha256,history_bytes,history_count,classification,declared,declaration_reason,provider,model,cached_tokens,uncached_tokens,expected_cached_tokens,since_previous_ms,warned,cache_write_tokens,output_tokens,reasoning_tokens,reasoning_effort,breakpoints,cache_lifetime_ms,likely_expired) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29)")?
                .execute(params![
                    row.session_id, row.lineage, row.seq as i64, row.created_at,
                    row.frozen.sha256, size(row.frozen.bytes), row.cold.sha256,
                    size(row.cold.bytes), size(row.cold.message_count), row.history.sha256,
                    size(row.history.bytes), size(row.history.message_count),
                    row.classification, row.declared, row.declaration_reason, row.provider,
                    row.model, number(row.cached_tokens), number(row.uncached_tokens),
                    number(row.expected_cached_tokens), number(row.since_previous_ms),
                    row.warned, number(row.cache_write_tokens), number(row.output_tokens),
                    number(row.reasoning_tokens), row.reasoning_effort, breakpoints,
                    number(row.cache_lifetime_ms), row.likely_expired,
                ])?;
            Ok(())
        })
    }
    /// Logged requests of a session, oldest first.
    pub fn model_requests(&self, session: &str) -> Result<Vec<RequestLogRow>> {
        let Some(view) = self.prompt_log_view() else {
            return Ok(Vec::new());
        };
        view.read(|conn| {
            let mut statement = conn.prepare("SELECT lineage,seq,created_at,frozen_sha256,frozen_bytes,cold_sha256,cold_bytes,cold_count,history_sha256,history_bytes,history_count,classification,declared,declaration_reason,provider,model,cached_tokens,uncached_tokens,expected_cached_tokens,since_previous_ms,warned,cache_write_tokens,output_tokens,reasoning_tokens,reasoning_effort,breakpoints,cache_lifetime_ms,likely_expired FROM model_requests WHERE session_id=?1 ORDER BY id")?;
            let rows = statement.query_map([session], |row| {
                let size = |index: usize| row.get::<_, i64>(index).map(|value| value as usize);
                let number = |index: usize| {
                    row.get::<_, Option<i64>>(index)
                        .map(|value| value.map(|value| value as u64))
                };
                let tier = |sha: usize, bytes: usize, count: Option<usize>| -> rusqlite::Result<_> {
                    Ok(TierFingerprint {
                        sha256: row.get(sha)?,
                        bytes: size(bytes)?,
                        message_count: count.map(size).transpose()?.unwrap_or(0),
                    })
                };
                Ok(RequestLogRow {
                    session_id: session.to_owned(),
                    lineage: row.get(0)?,
                    seq: row.get::<_, i64>(1)? as u64,
                    created_at: row.get(2)?,
                    frozen: tier(3, 4, None)?,
                    cold: tier(5, 6, Some(7))?,
                    history: tier(8, 9, Some(10))?,
                    classification: row.get(11)?,
                    declared: row.get(12)?,
                    declaration_reason: row.get(13)?,
                    provider: row.get(14)?,
                    model: row.get(15)?,
                    cached_tokens: number(16)?,
                    uncached_tokens: number(17)?,
                    expected_cached_tokens: number(18)?,
                    since_previous_ms: number(19)?,
                    warned: row.get(20)?,
                    cache_write_tokens: number(21)?,
                    output_tokens: number(22)?,
                    reasoning_tokens: number(23)?,
                    reasoning_effort: row.get(24)?,
                    breakpoints: row
                        .get::<_, Option<String>>(25)?
                        .and_then(|marks| serde_json::from_str(&marks).ok()),
                    cache_lifetime_ms: number(26)?,
                    likely_expired: row.get(27)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }
    /// The exact hashed bytes of a frozen or cold tier, while a row references it.
    pub fn prompt_tier(&self, sha256: &str) -> Result<Option<String>> {
        let Some(view) = self.prompt_log_view() else {
            return Ok(None);
        };
        view.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT content FROM prompt_tiers WHERE sha256=?1",
                    [sha256],
                    |row| row.get(0),
                )
                .optional()?)
        })
    }
}

/// Housekeeping's telemetry bound, which the prompt log shares: rows go with
/// their root's telemetry window, and a tier goes with its last row.
pub(in crate::store) fn prune(tx: &Connection, host_cutoff: &str) -> Result<()> {
    tx.execute("DELETE FROM telemetry WHERE EXISTS(SELECT 1 FROM root_housekeeping p WHERE p.session_id=telemetry.session_id AND telemetry.created_at<p.telemetry_cutoff) OR (telemetry.created_at<?1 AND NOT EXISTS(SELECT 1 FROM root_housekeeping p WHERE p.session_id=telemetry.session_id))", [host_cutoff])?;
    let removed = tx.execute("DELETE FROM model_requests WHERE EXISTS(SELECT 1 FROM root_housekeeping p WHERE p.session_id=model_requests.session_id AND model_requests.created_at<p.telemetry_cutoff) OR (model_requests.created_at<?1 AND NOT EXISTS(SELECT 1 FROM root_housekeeping p WHERE p.session_id=model_requests.session_id))", [host_cutoff])?;
    if removed > 0 {
        tx.execute("DELETE FROM prompt_tiers WHERE sha256 NOT IN (SELECT frozen_sha256 FROM model_requests UNION SELECT cold_sha256 FROM model_requests)", [])?;
    }
    Ok(())
}
