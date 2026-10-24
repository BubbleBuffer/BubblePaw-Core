//! Read-only quota reports; credentials and provider registration are not needed.
use anyhow::Result;
use bubblepaw_core::{QuotaReading, QuotaSource, Store};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::{io::Write, path::Path};

pub fn load(path: &Path, latest: bool, since: Option<&str>) -> Result<Vec<QuotaReading>> {
    let since = since.map(DateTime::parse_from_rfc3339).transpose()?;
    let mut rows = Store::read_quota_data_dir(path, latest)?;
    rows.retain(|row| {
        since.as_ref().is_none_or(|since| {
            DateTime::parse_from_rfc3339(&row.observed_at).is_ok_and(|at| at >= *since)
        })
    });
    rows.sort_by_key(|row| {
        (
            DateTime::parse_from_rfc3339(&row.observed_at).ok(),
            row.account.clone(),
            row.window.clone(),
            row.provider.clone(),
        )
    });
    Ok(rows)
}

#[derive(Serialize)]
struct Export<'a> {
    schema: &'static str,
    observed_at: &'a str,
    provider: &'a str,
    account: &'a str,
    window: &'a str,
    used_percent: f64,
    resets_at: &'a Option<String>,
    source: QuotaSource,
    balance: &'a Option<String>,
}

pub fn write_jsonl(rows: &[QuotaReading], writer: &mut impl Write) -> Result<()> {
    for row in rows {
        serde_json::to_writer(
            &mut *writer,
            &Export {
                schema: "bubblepaw.quota.v1",
                observed_at: &row.observed_at,
                provider: &row.provider,
                account: &row.account,
                window: &row.window,
                used_percent: row.used_percent,
                resets_at: &row.resets_at,
                source: row.source,
                balance: &row.balance,
            },
        )?;
        writeln!(writer)?;
    }
    Ok(())
}

pub fn write_latest(
    rows: &[QuotaReading],
    now: DateTime<Utc>,
    writer: &mut impl Write,
) -> Result<()> {
    write_latest_with_balances(rows, rows, now, writer)
}

pub fn write_latest_with_balances(
    rows: &[QuotaReading],
    balance_rows: &[QuotaReading],
    now: DateTime<Utc>,
    writer: &mut impl Write,
) -> Result<()> {
    if rows.is_empty() {
        writeln!(writer, "No quota readings recorded.")?;
        return Ok(());
    }
    writeln!(
        writer,
        "PROVIDER\tACCOUNT\tWINDOW\tUSED\tRESETS AT\tAGE\tSOURCE\tCREDIT BALANCE"
    )?;
    let mut rows: Vec<_> = rows.iter().collect();
    rows.sort_by_key(|row| (&row.account, &row.window));
    let mut balances = std::collections::BTreeMap::new();
    for row in balance_rows {
        if let Some(balance) = &row.balance {
            let at = DateTime::parse_from_rfc3339(&row.observed_at)?;
            let entry = balances
                .entry(&row.account)
                .or_insert((at, balance, &row.observed_at));
            if at >= entry.0 {
                *entry = (at, balance, &row.observed_at);
            }
        }
    }
    for row in &rows {
        let age = now
            .signed_duration_since(DateTime::parse_from_rfc3339(&row.observed_at)?)
            .num_seconds()
            .max(0);
        let source = serde_json::to_value(row.source)?;
        writeln!(
            writer,
            "{}\t{}\t{}\t{:.1}%\t{}\t{}s\t{}\t{}",
            row.provider,
            row.account,
            row.window,
            row.used_percent,
            row.resets_at.as_deref().unwrap_or("unknown"),
            age,
            source.as_str().unwrap_or("unknown"),
            if let Some((_, balance, at)) = balances.get(&row.account) {
                format!("balance {} as of {}", balance.escape_debug(), at)
            } else if row.window == "credits" {
                "unknown".to_owned()
            } else {
                "-".to_owned()
            }
        )?;
    }
    Ok(())
}
