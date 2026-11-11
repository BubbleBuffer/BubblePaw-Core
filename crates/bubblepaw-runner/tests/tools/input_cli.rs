use serde_json::{Value, json};
use std::process::{Command, Stdio};

#[test]
fn cli_prompt_character_limit_rejects_before_creating_an_archive() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("archive.sqlite");
    let output = Command::new(env!("CARGO_BIN_EXE_bubblepaw"))
        .args(["--provider", "demo", "--workspace"])
        .arg(root.path())
        .arg("--db")
        .arg(&db)
        .args(["--max-prompt-chars", "4", "run", "🐾🐾🐾🐾🐾"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("4 Unicode character limit"));
    assert!(
        !db.exists() && !bubblepaw_core::Store::legacy_data_dir(&db).exists(),
        "Rejected input must not create archive/session state"
    );
}

#[test]
fn stdio_reports_input_limits_and_rejects_oversized_text_without_admission() {
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_bubblepaw"))
        .args(["--provider", "demo", "--read-only", "--workspace"])
        .arg(root.path())
        .arg("--db")
        .arg(root.path().join("archive.sqlite"))
        .args(["--max-prompt-chars", "4", "stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for record in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
        json!({"jsonrpc":"2.0","id":2,"method":"run.start","params":{"session_id":"missing","text":"🐾🐾🐾🐾🐾"}}),
        json!({"jsonrpc":"2.0","id":3,"method":"shutdown"}),
    ] {
        writeln!(input, "{record}").unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let rejected = records.iter().find(|record| record["id"] == 2).unwrap();
    let initialized = records.iter().find(|record| record["id"] == 1).unwrap();
    assert_eq!(initialized["result"]["input_limits"]["prompt_chars"], 4);
    assert!(
        rejected["error"]["message"]
            .as_str()
            .unwrap()
            .contains("4 Unicode character limit")
    );
    let store = bubblepaw_core::Store::open(root.path().join("archive.sqlite")).unwrap();
    assert!(store.list_sessions().unwrap().is_empty());
}
