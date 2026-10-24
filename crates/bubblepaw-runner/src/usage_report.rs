//! Request-usage reports and JSONL export. Rows come from the authoritative store;
//! cost is computed here, at read time, from the dated table in `crate::pricing`.
use crate::pricing::{PriceTable, instant};
use anyhow::{Context, Result};
use bubblepaw_core::{RequestKind, RequestUsage, Store};
use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use std::{collections::BTreeMap, io::Write, path::Path};

pub const SCHEMA: &str = "bubblepaw.usage.v1";

/// One `usage export` line. Field order is the wire order.
#[derive(Serialize)]
struct ExportRow<'a> {
    schema: &'static str,
    request_id: &'a str,
    /// UTC with nanoseconds: a fixed 30-character RFC 3339 shape, precision kept.
    timestamp: String,
    session_id: &'a str,
    run_id: Option<&'a str>,
    provider: &'a str,
    account: &'a str,
    model: &'a str,
    kind: RequestKind,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: Option<u64>,
    failed: bool,
    cost_usd: Option<f64>,
    price_date: Option<&'a str>,
}

/// Validate a `--since` bound and normalise it to UTC with nanoseconds, so readers compare one format.
pub fn since_rfc3339(value: &str) -> Result<String> {
    let time = chrono::DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("--since must be an RFC 3339 time, got {value:?}"))?;
    Ok(time
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Nanos, true))
}

/// Retained rows, read without opening a host. A data directory with no chats yet has no usage.
pub fn load(
    data_dir: &Path,
    session: Option<&str>,
    since: Option<&str>,
) -> Result<Vec<RequestUsage>> {
    let rows = if data_dir.join("chats").is_dir() {
        Store::read_request_usage_data_dir(data_dir)?
    } else {
        Vec::new()
    };
    Ok(select(rows, session, since))
}

/// Rows for one session (exact match) and at or after `since`, compared as instants.
/// `since` is already validated RFC 3339; the bound is inclusive.
pub fn select(
    rows: Vec<RequestUsage>,
    session: Option<&str>,
    since: Option<&str>,
) -> Vec<RequestUsage> {
    let bound = since.and_then(instant);
    rows.into_iter()
        .filter(|row| session.is_none_or(|session| row.session_id == session))
        .filter(|row| {
            bound.is_none_or(|bound| instant(&row.created_at).is_some_and(|at| at >= bound))
        })
        .collect()
}

/// Time order by instant, then request id, so rows written with `Z` and `+01:00` forms tie correctly.
fn in_order(rows: &[RequestUsage]) -> Vec<&RequestUsage> {
    let mut ordered: Vec<&RequestUsage> = rows.iter().collect();
    ordered.sort_by(|a, b| {
        instant(&a.created_at)
            .cmp(&instant(&b.created_at))
            .then_with(|| a.request_id.cmp(&b.request_id))
    });
    ordered
}

fn utc_timestamp(value: &str) -> String {
    instant(value).map_or_else(
        || value.to_owned(),
        |at| at.to_rfc3339_opts(SecondsFormat::Nanos, true),
    )
}

fn export_row<'a>(row: &'a RequestUsage, prices: &'a PriceTable) -> ExportRow<'a> {
    let cost = prices.usage_cost(&row.model, &row.usage, instant(&row.created_at));
    ExportRow {
        schema: SCHEMA,
        request_id: &row.request_id,
        timestamp: utc_timestamp(&row.created_at),
        session_id: &row.session_id,
        run_id: row.run_id.as_deref(),
        provider: &row.provider,
        account: &row.account,
        model: &row.model,
        kind: row.kind,
        input_tokens: row.usage.input_tokens,
        cache_read_tokens: row.usage.cache_read_tokens,
        cache_creation_tokens: row.usage.cache_creation_tokens,
        output_tokens: row.usage.output_tokens,
        reasoning_tokens: row.usage.reasoning_tokens,
        failed: row.failed,
        cost_usd: cost,
        price_date: cost.map(|_| prices.checked.as_str()),
    }
}

/// One JSON object per request, ordered by time. No rows write nothing at all.
pub fn write_jsonl(rows: &[RequestUsage], prices: &PriceTable, mut out: impl Write) -> Result<()> {
    for row in in_order(rows) {
        serde_json::to_writer(&mut out, &export_row(row, prices))?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// Sums for one group. `cost_usd` is null while any request in the group is unpriced:
/// a partial sum is never reported as a total.
#[derive(Clone, Debug, Serialize)]
pub struct Totals {
    pub requests: u64,
    pub failed_requests: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    pub priced_cost_usd: f64,
    pub unpriced_requests: u64,
}
impl Default for Totals {
    fn default() -> Self {
        Self {
            requests: 0,
            failed_requests: 0,
            input_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: None,
            cost_usd: Some(0.0),
            priced_cost_usd: 0.0,
            unpriced_requests: 0,
        }
    }
}
impl Totals {
    fn add(&mut self, row: &RequestUsage, cost: Option<f64>) {
        self.requests += 1;
        self.failed_requests += u64::from(row.failed);
        self.input_tokens = self.input_tokens.saturating_add(row.usage.input_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(row.usage.cache_read_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(row.usage.cache_creation_tokens);
        self.output_tokens = self.output_tokens.saturating_add(row.usage.output_tokens);
        self.reasoning_tokens = match (self.reasoning_tokens, row.usage.reasoning_tokens) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or_default().saturating_add(b.unwrap_or_default())),
        };
        match cost {
            Some(cost) => self.priced_cost_usd += cost,
            None => self.unpriced_requests += 1,
        }
        self.cost_usd = (self.unpriced_requests == 0).then_some(self.priced_cost_usd);
    }
}

#[derive(Serialize)]
pub struct Group {
    pub key: String,
    #[serde(flatten)]
    pub totals: Totals,
}

#[derive(Serialize)]
pub struct Report {
    pub basis: &'static str,
    pub price_date: String,
    pub since: Option<String>,
    pub session: Option<String>,
    pub totals: Totals,
    pub by_session: Vec<Group>,
    pub by_model: Vec<Group>,
    pub by_provider: Vec<Group>,
    pub by_day: Vec<Group>,
}

fn groups(map: BTreeMap<String, Totals>) -> Vec<Group> {
    map.into_iter()
        .map(|(key, totals)| Group { key, totals })
        .collect()
}

/// Grouped totals by session, model, provider and UTC day, priced at read time.
pub fn report(
    rows: &[RequestUsage],
    prices: &PriceTable,
    since: Option<&str>,
    session: Option<&str>,
) -> Report {
    let mut totals = Totals::default();
    let mut by_session = BTreeMap::<String, Totals>::new();
    let mut by_model = BTreeMap::<String, Totals>::new();
    let mut by_provider = BTreeMap::<String, Totals>::new();
    let mut by_day = BTreeMap::<String, Totals>::new();
    for row in rows {
        let at = instant(&row.created_at);
        let cost = prices.usage_cost(&row.model, &row.usage, at);
        let day = at.map_or_else(
            || "unknown".to_owned(),
            |at| at.format("%Y-%m-%d").to_string(),
        );
        totals.add(row, cost);
        by_session
            .entry(row.session_id.clone())
            .or_default()
            .add(row, cost);
        by_model
            .entry(row.model.clone())
            .or_default()
            .add(row, cost);
        by_provider
            .entry(row.provider.clone())
            .or_default()
            .add(row, cost);
        by_day.entry(day).or_default().add(row, cost);
    }
    Report {
        basis: "authoritative request records, as retained (not an all-time bill); cost is computed at read time from the dated price table, and a request with no published rate is unpriced, never estimated",
        price_date: prices.checked.clone(),
        since: since.map(str::to_owned),
        session: session.map(str::to_owned),
        totals,
        by_session: groups(by_session),
        by_model: groups(by_model),
        by_provider: groups(by_provider),
        by_day: groups(by_day),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bubblepaw_core::{RequestKind, RequestUsage, Usage};
    use serde_json::Value;

    const KEYS: [&str; 17] = [
        "schema",
        "request_id",
        "timestamp",
        "session_id",
        "run_id",
        "provider",
        "account",
        "model",
        "kind",
        "input_tokens",
        "cache_read_tokens",
        "cache_creation_tokens",
        "output_tokens",
        "reasoning_tokens",
        "failed",
        "cost_usd",
        "price_date",
    ];

    fn usage(input: u64, cache_read: u64, output: u64, reasoning: Option<u64>) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_creation_tokens: 0,
            reasoning_tokens: reasoning,
        }
    }

    fn row(
        id: &str,
        created_at: &str,
        session: &str,
        provider: &str,
        model: &str,
        usage: Usage,
    ) -> RequestUsage {
        let mut row = RequestUsage::new(provider, model);
        row.request_id = id.into();
        row.created_at = created_at.into();
        row.session_id = session.into();
        row.usage = usage;
        row
    }

    /// Inserted out of order, so the sort does the work. Instants:
    /// a = 10-20 23:59:59.999999999Z; c = 10-21 00:00:00Z; d = the same instant written at
    /// +01:00; b = 10-21 00:00:00.000000001Z (one nanosecond after c and d).
    /// Time order is a, c, d, b: c and d tie on instant and fall back to the request id.
    fn sample() -> Vec<RequestUsage> {
        let mut b = row(
            "b",
            "2026-10-21T00:00:00.000000001Z",
            "session-b",
            "claude",
            "claude-haiku-5-5",
            usage(20, 10, 5, Some(1)),
        );
        b.kind = RequestKind::Decider;
        b.run_id = Some("run-b".into());
        b.failed = true;
        let d = row(
            "d",
            "2026-10-21T01:00:00+01:00",
            "session-b",
            "demo",
            "demo/unknown",
            usage(7, 0, 1, None),
        );
        let mut a = row(
            "a",
            "2026-10-20T23:59:59.999999999+00:00",
            "session-a",
            "opencode",
            "opencode/deepseek-v4.1-flash",
            usage(1000, 400, 100, None),
        );
        a.run_id = Some("run-a".into());
        let c = row(
            "c",
            "2026-10-21T00:00:00Z",
            "session-b",
            "demo",
            "demo/unknown",
            usage(7, 0, 1, None),
        );
        vec![b, d, a, c]
    }

    fn ids_in_order(rows: &[RequestUsage]) -> Vec<String> {
        in_order(rows)
            .iter()
            .map(|row| row.request_id.clone())
            .collect()
    }

    /// Strict wire check on one export line: exactly the seventeen keys, nullable fields present
    /// as null, and the value types. Key order is pinned by the callers' literal prefixes.
    fn assert_schema(line: &str) -> Value {
        let value: Value = serde_json::from_str(line).unwrap();
        let object = value.as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut expected = KEYS;
        expected.sort_unstable();
        assert_eq!(keys, expected, "{line}");
        assert_eq!(value["schema"], SCHEMA, "{line}");
        for key in [
            "request_id",
            "timestamp",
            "session_id",
            "provider",
            "account",
            "model",
            "kind",
        ] {
            assert!(value[key].is_string(), "{key}: {line}");
        }
        for key in [
            "input_tokens",
            "cache_read_tokens",
            "cache_creation_tokens",
            "output_tokens",
        ] {
            assert!(value[key].is_u64(), "{key}: {line}");
        }
        assert!(value["failed"].is_boolean(), "{line}");
        assert!(
            value["timestamp"].as_str().unwrap().ends_with('Z'),
            "{line}"
        );
        assert!(
            instant(value["timestamp"].as_str().unwrap()).is_some(),
            "{line}"
        );
        assert!(
            value["run_id"].is_null() || value["run_id"].is_string(),
            "{line}"
        );
        assert!(
            value["reasoning_tokens"].is_null() || value["reasoning_tokens"].is_u64(),
            "{line}"
        );
        assert!(
            value["cost_usd"].is_null() || value["cost_usd"].is_f64(),
            "{line}"
        );
        assert!(
            value["price_date"].is_null() || value["price_date"].is_string(),
            "{line}"
        );
        value
    }

    fn export(rows: &[RequestUsage]) -> String {
        let mut out = Vec::new();
        write_jsonl(rows, PriceTable::shared(), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn export_is_sorted_by_instant_then_request_id() {
        let text = export(&sample());
        assert!(text.ends_with('\n'));
        let lines: Vec<&str> = text.lines().collect();
        let ids: Vec<String> = lines
            .iter()
            .map(|line| {
                assert_schema(line)["request_id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(ids, ["a", "c", "d", "b"]);
    }

    #[test]
    fn export_line_pins_field_order_and_normalises_time_to_utc() {
        let text = export(&sample());
        let first = text.lines().next().unwrap();
        assert!(
            first.starts_with(
                r#"{"schema":"bubblepaw.usage.v1","request_id":"a","timestamp":"2026-10-20T23:59:59.999999999Z","session_id":"session-a","run_id":"run-a","provider":"opencode","account":"opencode","model":"opencode/deepseek-v4.1-flash","kind":"main","input_tokens":1000,"cache_read_tokens":400,"cache_creation_tokens":0,"output_tokens":100,"reasoning_tokens":null,"failed":false,"cost_usd":"#
            ),
            "{first}"
        );
        assert!(first.ends_with(r#","price_date":"2026-10-19"}"#), "{first}");
    }

    #[test]
    fn export_preserves_explicit_account_independent_of_provider() {
        let mut rows = sample();
        for row in &mut rows {
            row.account = "shared-subscription".into();
        }
        let text = export(&rows);
        for line in text.lines() {
            let value = assert_schema(line);
            assert_eq!(value["account"], "shared-subscription");
            assert!(line.contains(&format!(
                "\"provider\":{},\"account\":\"shared-subscription\",\"model\":",
                value["provider"]
            )));
        }
    }

    #[test]
    fn export_nullable_fields_are_present_as_null_and_typed() {
        let text = export(&sample());
        let lines: Vec<&str> = text.lines().collect();
        // Line 1 is c: no run, no reasoning, unpriced.
        let c = assert_schema(lines[1]);
        assert!(c["run_id"].is_null());
        assert!(c["reasoning_tokens"].is_null());
        assert!(c["cost_usd"].is_null());
        assert!(c["price_date"].is_null());
        // Line 3 is b: Haiku with reasoning, failed, and a priced cost.
        let b = assert_schema(lines[3]);
        assert_eq!(b["kind"], "decider");
        assert_eq!(b["reasoning_tokens"], 1);
        assert_eq!(b["failed"], true);
        assert_eq!(b["price_date"], "2026-10-19");
        assert!(b["cost_usd"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn timestamps_are_utc_with_nanoseconds_and_keep_their_instant() {
        assert_eq!(
            utc_timestamp("2026-10-21T01:00:00+01:00"),
            "2026-10-21T00:00:00.000000000Z"
        );
        assert_eq!(
            utc_timestamp("2026-10-20T23:59:59.999999999+00:00"),
            "2026-10-20T23:59:59.999999999Z"
        );
        assert_eq!(
            utc_timestamp("2026-10-21T00:00:00.000000001Z"),
            "2026-10-21T00:00:00.000000001Z"
        );
        assert_eq!(
            since_rfc3339("2026-10-21T14:00:00+02:00").unwrap(),
            "2026-10-21T12:00:00.000000000Z"
        );
        assert!(since_rfc3339("2026-10-21").is_err());
    }

    #[test]
    fn since_is_an_inclusive_bound_compared_as_instants() {
        let since_ids = |since: &str| {
            let bound = since_rfc3339(since).unwrap();
            ids_in_order(&select(sample(), None, Some(bound.as_str())))
        };
        // Equal to c, and to d written at +01:00: both kept, and a (one ns earlier) dropped.
        assert_eq!(since_ids("2026-10-21T00:00:00Z"), ["c", "d", "b"]);
        assert_eq!(since_ids("2026-10-21T01:00:00+01:00"), ["c", "d", "b"]);
        // One nanosecond after c and d: only b remains.
        assert_eq!(since_ids("2026-10-21T00:00:00.000000001Z"), ["b"]);
        // One nanosecond after b: nothing.
        assert_eq!(
            since_ids("2026-10-21T00:00:00.000000002Z"),
            Vec::<String>::new()
        );
        // Before everything: all four, in time order.
        assert_eq!(
            since_ids("2026-10-20T23:59:59.999999999Z"),
            ["a", "c", "d", "b"]
        );
    }

    #[test]
    fn session_filter_is_an_exact_match() {
        let rows = select(sample(), Some("session-b"), None);
        assert_eq!(ids_in_order(&rows), ["c", "d", "b"]);
        assert!(select(sample(), Some("session"), None).is_empty());
    }

    #[test]
    fn a_data_dir_without_chats_has_no_usage() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path(), None, None).unwrap().is_empty());
        let missing = dir.path().join("missing");
        assert!(
            load(&missing, None, Some("2026-10-21T00:00:00Z"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn report_totals_and_groups_null_the_cost_when_any_row_is_unpriced() {
        let rows = sample();
        let value = serde_json::to_value(report(
            &rows,
            PriceTable::shared(),
            Some("2026-10-20T00:00:00Z"),
            None,
        ))
        .unwrap();
        assert_eq!(value["since"], "2026-10-20T00:00:00Z");
        assert_eq!(value["price_date"], "2026-10-19");
        assert_eq!(value["totals"]["requests"], 4);
        assert_eq!(value["totals"]["failed_requests"], 1);
        assert_eq!(value["totals"]["input_tokens"], 1034);
        assert_eq!(value["totals"]["cache_read_tokens"], 410);
        assert_eq!(value["totals"]["output_tokens"], 107);
        assert_eq!(value["totals"]["reasoning_tokens"], 1);
        assert_eq!(value["totals"]["unpriced_requests"], 2);
        assert_eq!(value["totals"]["cost_usd"], Value::Null);
        let keys = |groups: &str| -> Vec<String> {
            value[groups]
                .as_array()
                .unwrap()
                .iter()
                .map(|group| group["key"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(keys("by_session"), ["session-a", "session-b"]);
        assert_eq!(
            keys("by_model"),
            [
                "claude-haiku-5-5",
                "demo/unknown",
                "opencode/deepseek-v4.1-flash"
            ]
        );
        assert_eq!(keys("by_day"), ["2026-10-20", "2026-10-21"]);
        assert_eq!(value["by_day"][1]["requests"], 3);
    }

    #[test]
    fn a_fully_priced_group_reports_its_sum() {
        let rows = select(sample(), Some("session-a"), None);
        let value =
            serde_json::to_value(report(&rows, PriceTable::shared(), None, Some("session-a")))
                .unwrap();
        // Off-peak Tuesday 23:59 UTC: 600 uncached at 0.15, 400 cache reads at 0.003, 100 output at 0.6 (per million).
        let expected = (600.0 * 0.15 + 400.0 * 0.003 + 100.0 * 0.6) / 1e6;
        let cost = value["totals"]["cost_usd"].as_f64().unwrap();
        assert!((cost - expected).abs() < 1e-12, "{cost}");
        assert_eq!(value["totals"]["unpriced_requests"], 0);
    }
}
