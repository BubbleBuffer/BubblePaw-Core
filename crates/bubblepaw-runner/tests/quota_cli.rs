use bubblepaw_core::{QuotaReading, QuotaSource, Store};
use serde_json::Value;
use std::{
    fs,
    process::{Command, Output},
};

struct Fixture {
    root: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("config.toml"),
            "[providers.codex]\nauth_file = 'missing-credentials.json'\n",
        )
        .unwrap();
        let store = Store::open_data_dir(root.path().join("data")).unwrap();
        for (at, used, source, provider) in [
            (
                "2026-10-20T12:00:00Z",
                25.0,
                QuotaSource::SdkEvent,
                "claude-a",
            ),
            (
                "2026-10-20T13:00:00Z",
                25.0,
                QuotaSource::UsageApi,
                "claude-alias",
            ),
            (
                "2026-10-20T14:00:00Z",
                100.0,
                QuotaSource::Rejection,
                "claude-a",
            ),
        ] {
            store
                .record_quota(&QuotaReading {
                    provider: provider.into(),
                    account: "shared".into(),
                    window: "five_hour".into(),
                    used_percent: used,
                    resets_at: Some("2026-10-20T17:00:00Z".into()),
                    observed_at: at.into(),
                    source,
                    balance: None,
                })
                .unwrap();
        }
        store
            .record_quota(&QuotaReading {
                provider: "codex".into(),
                account: "personal".into(),
                window: "primary".into(),
                used_percent: 17.5,
                resets_at: None,
                observed_at: "2026-10-20T15:00:00Z".into(),
                source: QuotaSource::ResponseHeaders,
                balance: None,
            })
            .unwrap();
        drop(store);
        Self { root }
    }
    fn cli(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("BUBBLEPAW_") {
                command.env_remove(key);
            }
        }
        command
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env("HOME", self.root.path().join("home"))
            .env("CODEX_HOME", self.root.path().join("codex"))
            .arg("--config")
            .arg(self.root.path().join("config.toml"))
            .arg("--workspace")
            .arg(self.root.path())
            .arg("--data-dir")
            .arg(self.root.path().join("data"));
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

#[test]
fn latest_is_offline_and_has_one_row_per_shared_account_window() {
    let fixture = Fixture::new();
    let before = fs::read(fixture.root.path().join("data/catalog.db")).unwrap();
    let text = success(fixture.cli().arg("quota").output().unwrap());
    assert!(text.contains("ACCOUNT") && text.contains("AGE") && text.contains("SOURCE"));
    assert_eq!(text.matches("shared").count(), 1);
    assert!(text.contains("100.0%") && text.contains("17.5%") && text.contains("unknown"));
    assert_eq!(
        before,
        fs::read(fixture.root.path().join("data/catalog.db")).unwrap()
    );
}

#[test]
fn export_contract_is_change_only_with_inclusive_offset_filter() {
    let fixture = Fixture::new();
    let text = success(fixture.cli().args(["quota", "export"]).output().unwrap());
    let rows: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 3);
    for row in &rows {
        let obj = row.as_object().unwrap();
        assert_eq!(obj.len(), 9);
        for key in [
            "schema",
            "observed_at",
            "provider",
            "account",
            "window",
            "used_percent",
            "resets_at",
            "source",
            "balance",
        ] {
            assert!(obj.contains_key(key), "missing {key}");
        }
        assert_eq!(row["schema"], "bubblepaw.quota.v1");
        assert!(row["balance"].is_null());
    }
    assert!(rows[2]["resets_at"].is_null());
    let filtered = success(
        fixture
            .cli()
            .args(["quota", "export", "--since", "2026-10-20T16:00:00+02:00"])
            .output()
            .unwrap(),
    );
    assert_eq!(filtered.lines().count(), 2);
    let empty = success(
        fixture
            .cli()
            .args(["quota", "export", "--since", "2026-10-21T00:00:00Z"])
            .output()
            .unwrap(),
    );
    assert!(empty.is_empty());
    assert!(
        !fixture
            .cli()
            .args(["quota", "export", "--since", "wrong"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn latest_formatter_shows_credit_balances_and_export_includes_them() {
    let mut credits = QuotaReading {
        provider: "codex".into(),
        account: "personal".into(),
        window: "credits".into(),
        used_percent: 0.0,
        resets_at: None,
        observed_at: "2026-10-21T00:00:00Z".into(),
        source: QuotaSource::ResponseHeaders,
        balance: Some("0012.500".into()),
    };
    let now = chrono::DateTime::parse_from_rfc3339(&credits.observed_at)
        .unwrap()
        .with_timezone(&chrono::Utc);
    for balance in ["0012.500", "unlimited"] {
        credits.balance = Some(balance.into());
        let mut table = Vec::new();
        bubblepaw_runner::quota_report::write_latest(&[credits.clone()], now, &mut table).unwrap();
        let text = String::from_utf8(table).unwrap();
        assert!(text.contains("CREDIT BALANCE"));
        assert!(text.contains(&format!("balance {balance} as of 2026-10-21T00:00:00Z")));
        let mut export = Vec::new();
        bubblepaw_runner::quota_report::write_jsonl(&[credits.clone()], &mut export).unwrap();
        let value: Value = serde_json::from_slice(&export).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 9);
        assert_eq!(value["balance"], balance);
    }
}

#[test]
fn standalone_balance_uses_latest_piggybacked_account_row_and_exports_it() {
    let fixture = Fixture::new();
    let store = Store::open_data_dir(fixture.root.path().join("data")).unwrap();
    let host = bubblepaw_runner::quota::QuotaHost::new(
        store.clone(),
        std::time::Duration::from_secs(3600),
    )
    .unwrap();
    let mut credits = QuotaReading {
        provider: "codex".into(),
        account: "personal".into(),
        window: "credits".into(),
        used_percent: 0.0,
        resets_at: None,
        observed_at: "2026-10-21T00:00:00Z".into(),
        source: QuotaSource::ResponseHeaders,
        balance: Some("100.00".into()),
    };
    for index in 0..10 {
        credits.observed_at = format!("2026-10-21T00:00:{index:02}Z");
        credits.balance = Some(format!("{}.00", 100 - index));
        host.observe("codex", "personal", &[credits.clone()])
            .unwrap();
    }
    let text = success(fixture.cli().arg("quota").output().unwrap());
    assert!(text.contains("balance 100.00 as of 2026-10-21T00:00:00Z"));
    assert!(!text.contains("balance 91.00"));
    assert_eq!(store.quota_history().unwrap().len(), 4);

    // The response's credit observation follows its secondary window. Its fresh
    // balance must still piggyback on the secondary row in this same batch.
    let mut secondary = credits.clone();
    secondary.window = "secondary".into();
    secondary.used_percent = 25.0;
    secondary.observed_at = "2026-10-21T00:00:10Z".into();
    secondary.balance = None;
    credits.observed_at = secondary.observed_at.clone();
    credits.balance = Some("90.00".into());
    host.observe("codex", "personal", &[secondary, credits])
        .unwrap();
    assert_eq!(store.quota_history().unwrap().len(), 5);
    drop(host);
    drop(store);

    let before = fs::read(fixture.root.path().join("data/catalog.db")).unwrap();
    let text = success(fixture.cli().arg("quota").output().unwrap());
    assert!(text.contains("balance 90.00 as of 2026-10-21T00:00:10Z"));
    assert!(!text.contains("balance 100.00"));
    let exported = success(fixture.cli().args(["quota", "export"]).output().unwrap());
    let rows: Vec<Value> = exported
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[3]["balance"], "100.00");
    assert_eq!(rows[4]["balance"], "90.00");
    assert_eq!(rows[4]["window"], "secondary");
    assert!(rows[..3].iter().all(|row| row["balance"].is_null()));
    assert!(rows.iter().all(|row| row["schema"] == "bubblepaw.quota.v1"));
    assert_eq!(
        before,
        fs::read(fixture.root.path().join("data/catalog.db")).unwrap()
    );
}

#[test]
fn standalone_balance_selects_last_stored_account_row_even_after_a_late_completion() {
    let fixture = Fixture::new();
    let store = Store::open_data_dir(fixture.root.path().join("data")).unwrap();
    let mut reading = QuotaReading {
        provider: "codex".into(),
        account: "personal".into(),
        window: "credits".into(),
        used_percent: 0.0,
        resets_at: None,
        observed_at: "2026-10-21T00:00:10Z".into(),
        source: QuotaSource::ResponseHeaders,
        balance: Some("100.00".into()),
    };
    store.record_quota(&reading).unwrap();
    reading.window = "secondary".into();
    reading.observed_at = "2026-10-21T00:00:05Z".into();
    reading.balance = Some("90.00".into());
    store.record_quota(&reading).unwrap();
    drop(store);
    let text = success(fixture.cli().arg("quota").output().unwrap());
    assert!(text.contains("balance 90.00 as of 2026-10-21T00:00:05Z"));
    assert!(!text.contains("balance 100.00"));
}
