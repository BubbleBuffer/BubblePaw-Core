use super::*;
use crate::{EventKind, QuotaReading, QuotaWait};

/// `balance` is the account balance known when a change row was written. Its
/// observation time is that row's `observed_at`.
pub(super) const QUOTA_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS quota_history(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    provider TEXT NOT NULL, account TEXT NOT NULL, window TEXT NOT NULL,
    data TEXT NOT NULL, balance TEXT);
    CREATE INDEX IF NOT EXISTS quota_history_key ON quota_history(account,window,id);";

fn has_table(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name=?2)",
        params![table, column],
        |row| row.get(0),
    )?)
}

#[derive(Clone, Copy)]
enum View {
    /// Every change row, oldest first.
    All,
    /// The newest row for each account window.
    Latest,
    /// The newest row carrying a balance, one per account.
    LatestBalances,
}

fn readings(conn: &Connection, view: View) -> Result<Vec<QuotaReading>> {
    if !has_table(conn, "quota_history")? {
        return Ok(Vec::new());
    }
    // Read-only CLI access may still see a v1 catalog without the column.
    let balance = if has_column(conn, "quota_history", "balance")? {
        "balance"
    } else {
        "NULL"
    };
    let sql = match view {
        View::All => format!("SELECT data,{balance} FROM quota_history ORDER BY id"),
        View::Latest => format!(
            "SELECT data,{balance} FROM quota_history WHERE id IN (SELECT MAX(id) FROM quota_history GROUP BY account,window) ORDER BY account,window"
        ),
        View::LatestBalances if balance == "NULL" => return Ok(Vec::new()),
        View::LatestBalances => "SELECT data,balance FROM quota_history WHERE id IN (SELECT MAX(id) FROM quota_history WHERE balance IS NOT NULL GROUP BY account) ORDER BY account".into(),
    };
    let mut statement = conn.prepare(&sql)?;
    statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .map(|row| {
            let (data, balance) = row?;
            let mut reading: QuotaReading = serde_json::from_str(&data)?;
            reading.balance = balance;
            Ok(reading)
        })
        .collect()
}

fn latest_for(conn: &Connection, reading: &QuotaReading) -> Result<Option<QuotaReading>> {
    if !has_table(conn, "quota_history")? {
        return Ok(None);
    }
    let data = conn.query_row("SELECT data FROM quota_history WHERE account=?1 AND window=?2 ORDER BY id DESC LIMIT 1",
        params![reading.account,reading.window], |row| row.get::<_, String>(0)).optional()?;
    data.map(|data| serde_json::from_str(&data).map_err(Into::into))
        .transpose()
}

fn record(conn: &mut Connection, reading: &QuotaReading) -> Result<bool> {
    if let Some(previous) = latest_for(conn, reading)? {
        // Last-seen age is host memory. Identical observations never begin a
        // write transaction or grow the WAL. Ignore arrivals older than the
        // retained change; hosts track newer unchanged observations in memory.
        if chrono::DateTime::parse_from_rfc3339(&reading.observed_at)?
            < chrono::DateTime::parse_from_rfc3339(&previous.observed_at)?
            || (previous.used_percent == reading.used_percent
                && previous
                    .resets_at
                    .as_deref()
                    .map(chrono::DateTime::parse_from_rfc3339)
                    .transpose()?
                    == reading
                        .resets_at
                        .as_deref()
                        .map(chrono::DateTime::parse_from_rfc3339)
                        .transpose()?)
        {
            return Ok(false);
        }
    }
    // The balance is the caller's latest known account balance, stored beside
    // the change. It is never written alone, so the JSON omits it.
    let mut historical = reading.clone();
    historical.balance = None;
    let tx = conn.transaction()?;
    tx.execute_batch(QUOTA_SCHEMA)?;
    // Callers supply fresher live metadata when available. If a response omits
    // it, carry the last stored account balance onto this already-needed row.
    let balance = match &reading.balance {
        Some(balance) => Some(balance.clone()),
        None => tx
            .query_row(
                "SELECT balance FROM quota_history WHERE account=?1 AND balance IS NOT NULL ORDER BY id DESC LIMIT 1",
                [&reading.account],
                |row| row.get::<_, String>(0),
            )
            .optional()?,
    };
    tx.execute(
        "INSERT INTO quota_history(provider,account,window,data,balance) VALUES(?1,?2,?3,?4,?5)",
        params![
            reading.provider,
            reading.account,
            reading.window,
            serde_json::to_string(&historical)?,
            balance
        ],
    )?;
    tx.commit()?;
    Ok(true)
}

const WAIT_PREFIX: &str = "quota_wait:";
fn wait_key(session: &str) -> String {
    format!("{WAIT_PREFIX}{session}")
}
fn read_wait(conn: &Connection, session: &str) -> Result<Option<QuotaWait>> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [wait_key(session)],
            |row| row.get(0),
        )
        .optional()?;
    value
        .map(|value| serde_json::from_str(&value).map_err(Into::into))
        .transpose()
}

impl Store {
    /// Durable data-directory-wide facts, deliberately independent of serving
    /// projections, session retention and individual chat archives.
    pub fn record_quota(&self, reading: &QuotaReading) -> Result<bool> {
        reading.validate()?;
        if let Some(host) = self.host() {
            return host.record_quota(reading);
        }
        if let Some(layout) = &self.0.layout {
            let changed = record(&mut layout.catalog.lock().unwrap(), reading)?;
            if changed {
                layout.catalog_transactions.fetch_add(1, Ordering::Relaxed);
            }
            return Ok(changed);
        }
        let changed = record(&mut self.0.connection.lock().unwrap(), reading)?;
        if changed {
            self.0.transactions.fetch_add(1, Ordering::Relaxed);
        }
        Ok(changed)
    }

    pub fn latest_quota(&self) -> Result<Vec<QuotaReading>> {
        self.quota_readings(View::Latest)
    }
    pub fn quota_history(&self) -> Result<Vec<QuotaReading>> {
        self.quota_readings(View::All)
    }
    /// The newest stored balance for each account, as carried by its change row.
    /// Hosts use it to resume the in-memory latest balance after reopening.
    pub fn latest_quota_balances(&self) -> Result<Vec<QuotaReading>> {
        self.quota_readings(View::LatestBalances)
    }
    fn quota_readings(&self, view: View) -> Result<Vec<QuotaReading>> {
        if let Some(host) = self.host() {
            return host.quota_readings(view);
        }
        if let Some(layout) = &self.0.layout {
            return readings(&layout.catalog.lock().unwrap(), view);
        }
        readings(&self.0.connection.lock().unwrap(), view)
    }

    /// Read only catalog.db, without acquiring store ownership, loading chats,
    /// restoring archives, starting providers or running schema migrations.
    pub fn read_quota_data_dir(
        data_dir: impl AsRef<Path>,
        latest: bool,
    ) -> Result<Vec<QuotaReading>> {
        Self::read_quota_view_data_dir(data_dir, if latest { View::Latest } else { View::All })
    }

    /// Read the last stored balance per account, ordered by insertion rather
    /// than observation time: a late completion can append an older observation.
    pub fn read_quota_balances_data_dir(data_dir: impl AsRef<Path>) -> Result<Vec<QuotaReading>> {
        Self::read_quota_view_data_dir(data_dir, View::LatestBalances)
    }

    fn read_quota_view_data_dir(
        data_dir: impl AsRef<Path>,
        view: View,
    ) -> Result<Vec<QuotaReading>> {
        let path = data_dir.as_ref().join("catalog.db");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let application: i64 = conn.query_row("PRAGMA application_id", [], |row| row.get(0))?;
        anyhow::ensure!(application == 0x42504231, "Unsupported quota catalog");
        readings(&conn, view)
    }

    pub fn quota_wait(&self, session_id: &str) -> Result<Option<QuotaWait>> {
        if let Some(store) = self.scoped(session_id)? {
            return store.quota_wait(session_id);
        }
        self.read(|conn| read_wait(conn, session_id))
    }

    /// Commit the marker and visible run status with their durable event. The
    /// caller must keep the original turn future alive while waiting.
    pub fn put_quota_wait(&self, wait: &QuotaWait) -> Result<()> {
        wait.validate()?;
        if let Some(host) = self.host() {
            return host.put_quota_wait(wait);
        }
        let cancellation = self
            .runtime()
            .quota_cancellation(&wait.session_id, &wait.run_id);
        self.0.runtime.log.commit(self, &wait.session_id, &wait.run_id, None, |tx| {
            anyhow::ensure!(cancellation.as_ref().is_none_or(|token| !token.is_cancelled()),
                "Cannot park a cancelled run");
            Self::ensure_quota_effects_resolved(tx, &wait.session_id)?;
            let status: Option<String> = tx.query_row("SELECT status FROM runs WHERE id=?1 AND session_id=?2",
                params![wait.run_id,wait.session_id], |row| row.get(0)).optional()?;
            anyhow::ensure!(status.as_deref().is_some_and(|s| matches!(s, "running"|"waiting_for_quota")), "Quota wait requires its active run");
            let other: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE session_id=?1 AND id!=?2 AND status IN ('running','waiting_for_quota'))",
                params![wait.session_id,wait.run_id], |row| row.get(0))?;
            anyhow::ensure!(!other, "Quota wait does not own the current run");
            tx.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![wait_key(&wait.session_id), serde_json::to_string(wait)?])?;
            tx.execute("UPDATE runs SET status='waiting_for_quota',data=json_set(data,'$.status','waiting_for_quota') WHERE id=?1", [&wait.run_id])?;
            Ok(((), vec![(crate::quota::waiting_event(wait), None)]))
        })
    }

    /// Remove one matching marker; stale waiters cannot clear a newer run's
    /// wait. Empty clears perform no write transaction and emit no event.
    pub fn clear_quota_wait(&self, session_id: &str, run_id: &str) -> Result<bool> {
        if let Some(host) = self.host() {
            return host.clear_quota_wait(session_id, run_id);
        }
        if self
            .quota_wait(session_id)?
            .is_none_or(|wait| wait.run_id != run_id)
        {
            return Ok(false);
        }
        self.0.runtime.log.commit(self, session_id, run_id, None, |tx| {
            let changed = Self::clear_quota_wait_tx(tx, session_id, Some(run_id))?;
            if changed {
                tx.execute("UPDATE runs SET status='running',data=json_set(data,'$.status','running') WHERE id=?1 AND status='waiting_for_quota'", [run_id])?;
            }
            Ok((changed, if changed { vec![(EventKind::RunQuotaResumed {}, None)] } else { Vec::new() }))
        })
    }

    /// Cancel a restored wait without launching its interrupted turn. Live
    /// harness cancellation instead drops the waiting future and finalizes it.
    pub(crate) fn cancel_quota_wait(&self, session_id: &str) -> Result<bool> {
        if let Some(host) = self.host() {
            return host.cancel_quota_wait(session_id);
        }
        let Some(wait) = self.quota_wait(session_id)? else {
            return Ok(false);
        };
        self.finish_quota_wait(
            session_id,
            &wait.run_id,
            "cancelled",
            "Cancelled while waiting for quota",
        )
    }

    /// Cancellation intent removes automatic recovery eligibility before the
    /// caller returns. Its original future still owns terminal run finalization.
    pub(crate) fn clear_cancelled_quota_wait(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> Result<bool> {
        if let Some(host) = self.host() {
            return host.clear_cancelled_quota_wait(session_id, run_id);
        }
        if self
            .quota_wait(session_id)?
            .is_none_or(|wait| wait.run_id != run_id)
        {
            return Ok(false);
        }
        self.0
            .runtime
            .log
            .commit(self, session_id, run_id, None, |tx| {
                let changed = Self::clear_quota_wait_tx(tx, session_id, Some(run_id))?;
                Ok((changed, Vec::new()))
            })
    }

    /// A startup scheduler could not safely admit the stored restart hint.
    /// Record the reason and remove the hint without starting any work. The
    /// owning live turn, if any, must instead be cancelled through its harness.
    pub fn fail_quota_wait(&self, session_id: &str, run_id: &str, reason: &str) -> Result<bool> {
        if let Some(host) = self.host() {
            return host.fail_quota_wait(session_id, run_id, reason);
        }
        anyhow::ensure!(
            !reason.trim().is_empty() && reason.len() <= 4096,
            "Quota failure reason must contain 1..=4096 bytes"
        );
        self.finish_quota_wait(session_id, run_id, "failed", reason)
    }

    fn finish_quota_wait(
        &self,
        session_id: &str,
        run_id: &str,
        status: &str,
        reason: &str,
    ) -> Result<bool> {
        if self
            .quota_wait(session_id)?
            .is_none_or(|wait| wait.run_id != run_id)
        {
            return Ok(false);
        }
        self.0
            .runtime
            .log
            .commit(self, session_id, run_id, None, |tx| {
                if !Self::clear_quota_wait_tx(tx, session_id, Some(run_id))? {
                    return Ok((false, Vec::new()));
                }
                let data: String =
                    tx.query_row("SELECT data FROM runs WHERE id=?1", [run_id], |row| {
                        row.get(0)
                    })?;
                let mut run = Self::run_from_data(tx, &data)?;
                anyhow::ensure!(
                    run.status == "interrupted",
                    "Quota recovery failure requires an interrupted run"
                );
                run.status = status.into();
                run.ended_at = Some(now());
                run.error = Some(reason.into());
                Self::finish_run_tx(tx, &run, None)?;
                Ok((
                    true,
                    vec![(
                        EventKind::RunFinished {
                            status: run.status,
                            text: None,
                            error: run.error,
                            usage: run.usage,
                        },
                        Some(crate::event_log::ContentRef::Run {
                            run_id: run_id.into(),
                        }),
                    )],
                ))
            })
    }

    pub(crate) fn clear_quota_wait_tx(
        tx: &Connection,
        session_id: &str,
        run_id: Option<&str>,
    ) -> Result<bool> {
        Ok(tx.execute("DELETE FROM metadata WHERE key=?1 AND (?2 IS NULL OR json_extract(value,'$.run_id')=?2)",
            params![wait_key(session_id), run_id])? > 0)
    }

    pub(crate) fn has_quota_wait_tx(tx: &Connection, session_id: &str) -> Result<bool> {
        Ok(read_wait(tx, session_id)?.is_some())
    }

    pub(crate) fn ensure_quota_resume_tx(tx: &Connection, expected: &QuotaWait) -> Result<()> {
        if read_wait(tx, &expected.session_id)?.as_ref() != Some(expected) {
            return Err(crate::QuotaWaitChanged {
                session_id: expected.session_id.clone(),
                run_id: expected.run_id.clone(),
            }
            .into());
        }
        let interrupted: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE id=?1 AND session_id=?2 AND status='interrupted')",
            params![expected.run_id, expected.session_id], |row| row.get(0))?;
        anyhow::ensure!(
            interrupted,
            "Automatic quota resume requires an interrupted run"
        );
        anyhow::ensure!(
            chrono::DateTime::parse_from_rfc3339(&expected.resume_at)? <= chrono::Utc::now(),
            "Quota wait is not due yet"
        );
        Ok(())
    }

    pub(crate) fn ensure_quota_effects_resolved(tx: &Connection, session_id: &str) -> Result<()> {
        let unresolved = Self::quota_effects_unresolved(tx, session_id)?;
        anyhow::ensure!(
            !unresolved,
            "Cannot park or automatically resume quota work with an unresolved external effect"
        );
        Ok(())
    }

    pub(crate) fn quota_effects_unresolved(tx: &Connection, session_id: &str) -> Result<bool> {
        Ok(tx.query_row("SELECT EXISTS(SELECT 1 FROM effects WHERE session_id=?1 AND state IN ('dispatched','unknown'))",
            [session_id], |row| row.get(0))?)
    }

    pub(crate) fn recover_quota_waits(&self) -> Result<()> {
        for wait in self.pending_quota_waits()? {
            let safe = self.read(|tx| {
                let interrupted: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE id=?1 AND session_id=?2 AND status='interrupted')",
                    params![wait.run_id, wait.session_id], |row| row.get(0))?;
                Ok(interrupted && !Self::quota_effects_unresolved(tx, &wait.session_id)?)
            })?;
            if !safe {
                self.write(|tx| {
                    Self::clear_quota_wait_tx(tx, &wait.session_id, Some(&wait.run_id))
                })?;
            }
        }
        Ok(())
    }

    /// Startup recovery candidates, including hidden child sessions. Archived
    /// chats are never restored by this query; waits block automatic archival.
    pub fn pending_quota_waits(&self) -> Result<Vec<QuotaWait>> {
        if let Some(layout) = &self.0.layout {
            let sessions = {
                let catalog = layout.catalog.lock().unwrap();
                let mut statement = catalog
                    .prepare("SELECT id FROM sessions WHERE storage_state='active' ORDER BY id")?;
                statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut waits = Vec::new();
            for session in sessions {
                if let Some(wait) = self.quota_wait(&session)? {
                    waits.push(wait);
                }
            }
            return Ok(waits);
        }
        self.read(|conn| {
            let mut statement = conn.prepare(
                "SELECT value FROM metadata WHERE substr(key,1,11)='quota_wait:' ORDER BY key",
            )?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .map(|row| Ok(serde_json::from_str(&row?)?))
                .collect()
        })
    }
}
