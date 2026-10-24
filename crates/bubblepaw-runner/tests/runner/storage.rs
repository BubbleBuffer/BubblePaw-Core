use super::*;

#[test]
fn native_demo_persists_and_reopens_a_session() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let db = temp.path().join("archive.sqlite");
    let first = cli(&workspace, &db)
        .args(["run", "Exercise the native tools"])
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let records: Vec<Value> = String::from_utf8(first.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let result = records.iter().find(|v| v["type"] == "result").unwrap();
    let session = result["session_id"].as_str().unwrap();
    assert_eq!(result["run"]["status"], "completed");
    assert_eq!(
        records
            .iter()
            .filter(|v| v["type"] == "tool_finished" && v["name"] == "js")
            .count(),
        0
    );
    for name in ["list", "write", "read", "exec"] {
        assert!(
            records
                .iter()
                .any(|v| v["type"] == "tool_finished" && v["name"] == name)
        );
    }
    assert!(
        records
            .iter()
            .filter(|v| v["type"] == "tool_finished")
            .all(|v| v["result"]["is_error"] == false)
    );
    let file = std::fs::read_dir(&workspace)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("bubblepaw-demo-")
        })
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(file).unwrap(),
        "BubblePaw base executed a native tool.\n"
    );
    let second = cli(&workspace, &db)
        .args(["--read-only", "run", "Continue", "--session", session])
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let history = cli(&workspace, &db)
        .args(["history", session])
        .output()
        .unwrap();
    assert!(history.status.success());
    let messages: Vec<Value> = serde_json::from_slice(&history.stdout).unwrap();
    let typed = messages
        .iter()
        .filter(|m| m["role"] == "user" && m["origin"] == "typed")
        .collect::<Vec<_>>();
    assert_eq!(typed.len(), 2);
    assert!(
        typed
            .iter()
            .any(|m| m["content"] == "Exercise the native tools")
    );
    assert!(typed.iter().any(|m| m["content"] == "Continue"));
    for message in messages
        .iter()
        .filter(|m| m["role"] == "user" && m["origin"] != "typed")
    {
        assert_eq!(message["origin"], "generated", "{message}");
        let content = message["content"].as_str().unwrap();
        assert!(content.starts_with("<harness "), "{message}");
    }
    assert!(
        messages
            .iter()
            .flat_map(|m| m["tool_calls"].as_array().into_iter().flatten())
            .all(|call| ["list", "write", "read", "exec"].contains(&call["name"].as_str().unwrap()))
    );
    let stats = cli(&workspace, &db).arg("stats").output().unwrap();
    assert!(stats.status.success());
    let stats: Value = serde_json::from_slice(&stats.stdout).unwrap();
    assert!(
        // Chat-persistent native declarations and inventory baselines now join
        // the disposable serving projection. Serial Linux acceptance measured
        // total 616 KiB, chat 416 KiB, serving 164 KiB and WAL 49,504 bytes.
        // Retain separate, bounded headroom for each storage category.
        stats["database_bytes"].as_u64().unwrap() < 640 * 1024,
        "{stats}"
    );
    assert!(
        stats["chat_bytes"].as_u64().unwrap() < 448 * 1024,
        "{stats}"
    );
    assert!(
        stats["serving_bytes"].as_u64().unwrap() < 192 * 1024,
        "{stats}"
    );
    assert!(stats["wal_bytes"].as_u64().unwrap() < 64 * 1024, "{stats}");
    assert!(stats["message_count"].as_u64().unwrap() <= 16, "{stats}");
    assert_eq!(stats["pending_input_count"], 0);
}
