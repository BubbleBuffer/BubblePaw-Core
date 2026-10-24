use serde_json::Value;
use std::process::Command;

#[test]
fn cli_trace_modes_report_and_default_off() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let binary = env!("CARGO_BIN_EXE_bubblepaw");
    for mode in ["off", "metadata", "contents"] {
        let db = temp.path().join(format!("{mode}.sqlite"));
        let mut command = Command::new(binary);
        command
            .args(["--provider", "demo", "--workspace"])
            .arg(&root)
            .arg("--db")
            .arg(db);
        let path = root.join(format!("{mode}.jsonl"));
        if mode != "off" {
            command.args([
                "--trace-file",
                &format!("{mode}.jsonl"),
                "--trace-mode",
                mode,
                "--trace-label",
                "cli-verification",
            ]);
        }
        let output = command
            .args(["run", "private-observation-sentinel"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if mode == "off" {
            assert!(!path.exists());
            assert!(!String::from_utf8_lossy(&output.stderr).contains("tracing enabled"));
            continue;
        }
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            contents.contains("private-observation-sentinel"),
            mode == "contents"
        );
        for line in contents.lines() {
            serde_json::from_str::<Value>(line).unwrap();
        }
        assert!(contents.contains("source_fingerprint"));
        let report = Command::new(binary)
            .arg("trace-report")
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            report.status.success(),
            "{}",
            String::from_utf8_lossy(&report.stderr)
        );
        let report: Value = serde_json::from_slice(&report.stdout).unwrap();
        assert!(report["model_calls"].as_u64().unwrap() >= 2);
        assert_eq!(report["tool_errors"], 0);
        assert_eq!(report["malformed_records"], 0);
        assert_eq!(report["incomplete_calls"], 0);
        assert_eq!(report["write_errors"], 0);
    }
}

#[test]
fn trace_paths_cannot_target_archive_or_sidecars_including_rotation() {
    let temp = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_bubblepaw");
    for name in [
        "archive.sqlite",
        "archive.sqlite-wal",
        "archive.sqlite-shm",
        "trace.jsonl",
    ] {
        let db = temp.path().join(if name == "trace.jsonl" {
            "trace.jsonl.1"
        } else {
            "archive.sqlite"
        });
        let output = Command::new(binary)
            .args(["--provider", "demo", "--workspace"])
            .arg(temp.path())
            .arg("--db")
            .arg(&db)
            .arg("--trace-file")
            .arg(temp.path().join(name))
            .args(["run", "must be rejected before execution"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("must be separate"));
        let catalog = bubblepaw_core::Store::legacy_data_dir(&db).join("catalog.db");
        assert_eq!(&std::fs::read(catalog).unwrap()[..16], b"SQLite format 3\0");
    }
}

#[test]
fn trace_paths_cannot_target_split_store_files_or_chat_directory() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    for relative in [
        "catalog.db",
        "catalog.db-wal",
        "serving.db",
        "serving.db-shm",
        "store.lock",
        "chats/new.db",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_bubblepaw"))
            .args(["--provider", "demo", "--workspace"])
            .arg(temp.path())
            .arg("--data-dir")
            .arg(&data)
            .arg("--trace-file")
            .arg(data.join(relative))
            .args(["run", "must be rejected before execution"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("must be separate"));
    }
    assert!(!data.join("chats/new.db").exists());
}
