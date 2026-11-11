//! `bubblepaw usage` and `usage export` run offline against a fixture data directory.
use bubblepaw_core::{RequestUsage, Store, Usage};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

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

struct Fixture {
    root: tempfile::TempDir,
    data: PathBuf,
    first: String,
    second: String,
}

impl Fixture {
    /// Three requests: two sessions, one unknown model, one failed Haiku request, and
    /// a timestamp written with a non-UTC offset to check the instant comparison.
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        fs::write(
            root.path().join("config.toml"),
            "[providers.codex]\nauth_file = 'missing-credentials.json'\n",
        )
        .unwrap();
        let store = Store::open_data_dir(&data).unwrap();
        let first = store.create_session("first").unwrap().id;
        let second = store.create_session("second").unwrap().id;
        let record = |id: &str, session: &str, created_at: &str, model: &str, usage: Usage| {
            let mut record = RequestUsage::new("opencode", model);
            record.request_id = id.into();
            record.created_at = created_at.into();
            record.session_id = session.into();
            record.run_id = Some(format!("run-{session}"));
            record.usage = usage;
            record
        };
        let usage = |input, cache_read, output| Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_creation_tokens: 0,
            reasoning_tokens: None,
        };
        store
            .queue_request_usage(record(
                "r-deepseek",
                &first,
                "2026-10-20T10:00:00Z",
                "opencode/deepseek-v4.1-flash",
                usage(1000, 400, 100),
            ))
            .unwrap();
        store.flush_request_usage(&first).unwrap();
        let mut failed = record(
            "r-haiku",
            &second,
            "2026-10-21T12:00:00.000000000Z",
            "claude-haiku-5-5",
            usage(50, 0, 5),
        );
        failed.failed = true;
        failed.account = "shared-subscription".into();
        store.queue_request_usage(failed).unwrap();
        store
            .queue_request_usage(record(
                "r-unknown",
                &second,
                "2026-10-22T00:00:00Z",
                "demo/unknown",
                usage(7, 0, 1),
            ))
            .unwrap();
        store.flush_request_usage(&second).unwrap();
        drop(store);
        Self {
            root,
            data,
            first,
            second,
        }
    }

    fn cli(&self) -> Command {
        self.cli_for(&self.data)
    }

    fn cli_for(&self, data: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("BUBBLEPAW_") {
                command.env_remove(key);
            }
        }
        command
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENCODE_API_KEY")
            .env("HOME", self.root.path().join("home"))
            .env("CODEX_HOME", self.root.path().join("codex"))
            .arg("--config")
            .arg(self.root.path().join("config.toml"))
            .arg("--workspace")
            .arg(self.root.path())
            .arg("--data-dir")
            .arg(data);
        command
    }
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn ids(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| {
            let value: Value = serde_json::from_str(line).unwrap();
            value["request_id"].as_str().unwrap().to_owned()
        })
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
    assert_eq!(value["schema"], "bubblepaw.usage.v1", "{line}");
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

#[test]
fn export_is_exact_jsonl_ordered_by_time() {
    let fixture = Fixture::new();
    let text = success(fixture.cli().args(["usage", "export"]).output().unwrap());
    assert!(text.ends_with('\n'));
    assert_eq!(ids(&text), ["r-deepseek", "r-haiku", "r-unknown"]);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3);
    for line in &lines {
        assert_schema(line);
    }

    // Field order is the wire contract: the literal prefix pins it, then cost and price date follow.
    let deepseek = format!(
        r#"{{"schema":"bubblepaw.usage.v1","request_id":"r-deepseek","timestamp":"2026-10-20T10:00:00.000000000Z","session_id":"{first}","run_id":"run-{first}","provider":"opencode","account":"opencode","model":"opencode/deepseek-v4.1-flash","kind":"main","input_tokens":1000,"cache_read_tokens":400,"cache_creation_tokens":0,"output_tokens":100,"reasoning_tokens":null,"failed":false,"cost_usd":"#,
        first = fixture.first
    );
    assert!(lines[0].starts_with(&deepseek), "{}", lines[0]);
    assert!(
        lines[0].ends_with(r#","price_date":"2026-10-19"}"#),
        "{}",
        lines[0]
    );

    let haiku = format!(
        r#"{{"schema":"bubblepaw.usage.v1","request_id":"r-haiku","timestamp":"2026-10-21T12:00:00.000000000Z","session_id":"{second}","run_id":"run-{second}","provider":"opencode","account":"shared-subscription","model":"claude-haiku-5-5","kind":"main","input_tokens":50,"cache_read_tokens":0,"cache_creation_tokens":0,"output_tokens":5,"reasoning_tokens":null,"failed":true,"cost_usd":"#,
        second = fixture.second
    );
    assert!(lines[1].starts_with(&haiku), "{}", lines[1]);
    let parsed = assert_schema(lines[1]);
    assert_eq!(parsed["kind"], "main");
    assert_eq!(parsed["price_date"], "2026-10-19");
    assert!(parsed["cost_usd"].as_f64().unwrap() > 0.0);

    let unknown = assert_schema(lines[2]);
    assert_eq!(unknown["cost_usd"], Value::Null);
    assert_eq!(unknown["price_date"], Value::Null);
    assert_eq!(unknown["run_id"], format!("run-{}", fixture.second));
    assert!(
        lines[2].ends_with(r#","cost_usd":null,"price_date":null}"#),
        "{}",
        lines[2]
    );
}

#[test]
fn since_is_an_inclusive_instant_bound() {
    let fixture = Fixture::new();
    // Equal to the Haiku row's instant; the same instant written with an offset.
    for since in ["2026-10-21T12:00:00Z", "2026-10-21T14:00:00+02:00"] {
        let text = success(
            fixture
                .cli()
                .args(["usage", "export", "--since", since])
                .output()
                .unwrap(),
        );
        assert_eq!(ids(&text), ["r-haiku", "r-unknown"], "since {since}");
    }
    let later = success(
        fixture
            .cli()
            .args([
                "usage",
                "export",
                "--since",
                "2026-10-21T12:00:00.000000001Z",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(ids(&later), ["r-unknown"]);
}

#[test]
fn empty_export_succeeds_with_no_stdout() {
    let fixture = Fixture::new();
    let text = success(
        fixture
            .cli()
            .args(["usage", "export", "--since", "2030-01-01T00:00:00Z"])
            .output()
            .unwrap(),
    );
    assert_eq!(text, "");
}

#[test]
fn missing_data_dir_exports_nothing_and_reports_zero() {
    let fixture = Fixture::new();
    let fresh = fixture.root.path().join("fresh");
    let text = success(
        fixture
            .cli_for(&fresh)
            .args(["usage", "export"])
            .output()
            .unwrap(),
    );
    assert_eq!(text, "");
    let report: Value = serde_json::from_str(&success(
        fixture.cli_for(&fresh).args(["usage"]).output().unwrap(),
    ))
    .unwrap();
    assert_eq!(report["totals"]["requests"], 0);
    assert_eq!(report["by_day"].as_array().unwrap().len(), 0);
}

#[test]
fn invalid_since_is_rejected() {
    let fixture = Fixture::new();
    let output = fixture
        .cli()
        .args(["usage", "export", "--since", "2026-10-21"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("RFC 3339"));
    assert!(output.stdout.is_empty());
}

#[test]
fn report_groups_by_session_model_provider_and_day() {
    let fixture = Fixture::new();
    let report: Value =
        serde_json::from_str(&success(fixture.cli().args(["usage"]).output().unwrap())).unwrap();
    assert_eq!(report["totals"]["requests"], 3);
    assert_eq!(report["totals"]["failed_requests"], 1);
    assert_eq!(report["totals"]["input_tokens"], 1057);
    assert_eq!(report["totals"]["cache_read_tokens"], 400);
    assert_eq!(report["totals"]["output_tokens"], 106);
    assert_eq!(report["totals"]["unpriced_requests"], 1);
    assert_eq!(report["totals"]["cost_usd"], Value::Null);
    assert_eq!(report["by_day"].as_array().unwrap().len(), 3);
    assert_eq!(report["by_provider"][0]["key"], "opencode");
    assert_eq!(report["by_provider"][0]["requests"], 3);
    assert_eq!(report["by_session"].as_array().unwrap().len(), 2);
    assert_eq!(report["price_date"], "2026-10-19");

    let session: Value = serde_json::from_str(&success(
        fixture
            .cli()
            .args(["usage", "--session", fixture.first.as_str()])
            .output()
            .unwrap(),
    ))
    .unwrap();
    assert_eq!(session["session"], fixture.first);
    assert_eq!(session["totals"]["requests"], 1);
    assert!(session["totals"]["cost_usd"].as_f64().unwrap() > 0.0);
    assert_eq!(
        session["by_model"][0]["key"],
        "opencode/deepseek-v4.1-flash"
    );
}
