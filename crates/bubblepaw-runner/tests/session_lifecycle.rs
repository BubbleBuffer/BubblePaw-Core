//! Offline acceptance of names, isolated runs and archive-only retention.
use bubblepaw_core::{ContextStrategy, Store};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

fn cli(root: &Path, data: &Path) -> Command {
    let config = root.join("session-config.toml");
    if !config.exists() {
        fs::write(&config, "").unwrap();
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
    command.args(["--set", "agent.execution_mode='js'"]);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("BUBBLEPAW_") {
            command.env_remove(key);
        }
    }
    command
        .env("HOME", root.join("home"))
        .env("CODEX_HOME", root.join("codex"))
        .args(["--provider", "demo", "--no-auto-summarize", "--config"])
        .arg(config)
        .arg("--workspace")
        .arg(root)
        .arg("--data-dir")
        .arg(data);
    command
}
fn success(output: Output) -> Vec<u8> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}
fn files(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if !path.exists() {
            return;
        }
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, result);
            } else {
                result.insert(
                    path.strip_prefix(root).unwrap().into(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(path, path, &mut result);
    result
}
fn rpc(root: &Path, data: &Path, requests: &[Value]) -> Vec<Value> {
    let mut child = cli(root, data)
        .arg("stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for request in requests {
        let mut request = request.clone();
        request["jsonrpc"] = json!("2.0");
        writeln!(input, "{request}").unwrap();
    }
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","id":999,"method":"shutdown"})
    )
    .unwrap();
    drop(input);
    String::from_utf8(success(child.wait_with_output().unwrap()))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn response(records: &[Value], id: u64) -> &Value {
    let response = records.iter().find(|value| value["id"] == id).unwrap();
    assert!(response.get("error").is_none(), "{response}");
    &response["result"]
}

#[test]
fn names_create_rename_list_search_and_reopen_without_conversation_input() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let script = root.path().join("empty.js");
    fs::write(&script, "return 'searchable';").unwrap();
    success(
        cli(root.path(), &data)
            .args(["script"])
            .arg(&script)
            .args(["--name", "Named script"])
            .output()
            .unwrap(),
    );
    let store = Store::open_data_dir(&data).unwrap();
    let session = store.list_sessions().unwrap().remove(0);
    assert_eq!(session.title, "Named script");
    let before = store.messages(&session.id).unwrap().len();
    drop(store);
    let records = rpc(
        root.path(),
        &data,
        &[
            json!({"id":1,"method":"session.rename","params":{"session_id":session.id,"title":"API name"}}),
            json!({"id":2,"method":"run.start","params":{"session_id":session.id,"text":"/rename Final name"}}),
            json!({"id":3,"method":"session.list"}),
            json!({"id":4,"method":"session.state","params":{"session_id":session.id}}),
            json!({"id":5,"method":"session.rename","params":{"session_id":session.id,"title":"  "}}),
        ],
    );
    assert_eq!(response(&records, 1)["title"], "API name");
    assert_eq!(response(&records, 2)["title"], "Final name");
    assert_eq!(response(&records, 3)[0]["title"], "Final name");
    assert!(response(&records, 3)[0]["usage"]["context"].is_object());
    assert!(response(&records, 3)[0]["usage"]["own"].is_object());
    assert_eq!(
        response(&records, 4)["messages"].as_array().unwrap().len(),
        before
    );
    assert!(
        response(&records, 4)["usage"]["context"]
            .get("last_request_input_tokens")
            .is_some()
    );
    assert!(response(&records, 4)["usage"]["total"].is_object());
    assert!(
        records
            .iter()
            .find(|value| value["id"] == 5)
            .unwrap()
            .get("error")
            .is_some()
    );
    let hits: Value = serde_json::from_slice(&success(
        cli(root.path(), &data)
            .args(["search", "searchable"])
            .output()
            .unwrap(),
    ))
    .unwrap();
    assert!(!hits.as_array().unwrap().is_empty());
    assert_eq!(hits[0]["title"], "Final name");
    success(
        cli(root.path(), &data)
            .args(["run", "/rename CLI name", "--session", &session.id])
            .output()
            .unwrap(),
    );
    let store = Store::open_data_dir(&data).unwrap();
    assert_eq!(store.session(&session.id).unwrap().title, "CLI name");
    assert_eq!(store.messages(&session.id).unwrap().len(), before);
    drop(store);
    let opened = rpc(
        root.path(),
        &data,
        &[
            json!({"id":1,"method":"session.open"}),
            json!({"id":2,"method":"session.open","params":{"name":"Alias name"}}),
        ],
    );
    assert_eq!(response(&opened, 1)["title"], "New session");
    assert_eq!(response(&opened, 2)["title"], "Alias name");
    success(
        cli(root.path(), &data)
            .args(["run", "hello", "--name", "Named run"])
            .output()
            .unwrap(),
    );
    assert!(
        Store::open_data_dir(&data)
            .unwrap()
            .list_sessions()
            .unwrap()
            .iter()
            .any(|session| session.title == "Named run")
    );
    let stats: Value = serde_json::from_slice(&success(
        cli(root.path(), &data).arg("stats").output().unwrap(),
    ))
    .unwrap();
    assert!(stats.get("database_bytes").is_some());
    assert!(
        stats["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|session| session["usage"]["context"].is_object())
    );
}

#[test]
fn ephemeral_run_and_script_leave_every_real_store_byte_untouched_and_remove_spills() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("real-store");
    let store = Store::open_data_dir(&data).unwrap();
    store.create_session("retain me").unwrap();
    drop(store);
    let before = files(&data);
    let script = root.path().join("large.js");
    fs::write(&script, "return 'x'.repeat(100000);").unwrap();
    let output = cli(root.path(), &data)
        .arg("script")
        .arg(&script)
        .args(["--no-session", "--name", "temporary"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(success(output)).unwrap();
    // Large script returns are spilled by CodeMode; their emitted reference
    // proves all session resources used the temporary tree, then disappeared.
    assert!(stdout.contains("bubblepaw-ephemeral-"), "{stdout}");
    let value: Value = serde_json::from_str(&stdout).unwrap();
    fn find_path(value: &Value) -> Option<&str> {
        match value {
            Value::Object(map) => map
                .get("path")
                .and_then(Value::as_str)
                .filter(|path| path.contains("bubblepaw-ephemeral-"))
                .or_else(|| map.values().find_map(find_path)),
            Value::Array(values) => values.iter().find_map(find_path),
            _ => None,
        }
    }
    let spill = find_path(&value).expect("temporary spill reference");
    assert!(!Path::new(spill).exists());
    let temporary_root = Path::new(spill)
        .ancestors()
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("bubblepaw-ephemeral-"))
        })
        .unwrap();
    assert!(
        !temporary_root.exists(),
        "entire temporary store must disappear"
    );
    assert_eq!(files(&data), before);
    success(
        cli(root.path(), &data)
            .args(["run", "hello", "--no-session", "--name", "temporary run"])
            .output()
            .unwrap(),
    );
    assert_eq!(files(&data), before);
    let missing = root.path().join("never-create");
    let failing = root.path().join("fail.js");
    fs::write(&failing, "throw new Error('fixture failure');").unwrap();
    let output = cli(root.path(), &missing)
        .arg("script")
        .arg(failing)
        .arg("--no-session")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!missing.exists());
    let conflict = cli(root.path(), &data)
        .args(["run", "hello", "--no-session", "--session", "existing"])
        .output()
        .unwrap();
    assert!(!conflict.status.success());
    assert_eq!(files(&data), before);
    // A configured legacy migration source must remain unopened as well.
    let legacy_root = root.path().join("legacy");
    fs::create_dir(&legacy_root).unwrap();
    let legacy = legacy_root.join("archive.sqlite");
    fs::write(&legacy, b"not SQLite; must never be opened").unwrap();
    let legacy_before = files(&legacy_root);
    let output = Command::new(env!("CARGO_BIN_EXE_bubblepaw"))
        .args(["--set", "agent.execution_mode='js'"])
        .args(["--provider", "demo", "--no-auto-summarize", "--config"])
        .arg(root.path().join("session-config.toml"))
        .arg("--workspace")
        .arg(root.path())
        .arg("--db")
        .arg(&legacy)
        .arg("script")
        .arg(&script)
        .arg("--no-session")
        .output()
        .unwrap();
    success(output);
    assert_eq!(files(&legacy_root), legacy_before);
}

fn age_catalog(data: &Path, root: &str, time: &str) {
    let connection = rusqlite::Connection::open(data.join("catalog.db")).unwrap();
    connection
        .execute(
            "UPDATE metadata SET value=?1 WHERE key=?2",
            rusqlite::params![time, format!("archived_at:{root}")],
        )
        .unwrap();
}

#[test]
fn startup_retention_deletes_only_expired_archives_and_their_complete_resource_tree() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let store = Store::open_data_dir(&data).unwrap();
    let expired = store.create_session("expired").unwrap();
    let child = store
        .create_session_with_parent("descendant", ContextStrategy::Lcm, Some(&expired.id))
        .unwrap();
    let active = store.create_session("old but active").unwrap();
    let recent = store.create_session("recent archive").unwrap();
    let unknown = store.create_session("legacy archive without date").unwrap();
    store
        .store_attachment(
            &expired.id,
            "file-checkpoint/fixture",
            "{}",
            b"checkpoint preimage",
        )
        .unwrap();
    store
        .store_attachment(&child.id, "binary", "{}", b"child image bytes")
        .unwrap();
    let spills = bubblepaw_runner::spill::SpillStore::new(&data).unwrap();
    spills.attach(&store);
    spills.save(&expired.id, "root spill").unwrap();
    spills.save(&child.id, "child spill").unwrap();
    for session in [&expired, &recent, &unknown] {
        assert!(store.archive_chat(&session.id).unwrap());
    }
    drop(store);
    fs::write(
        root.path().join("session-config.toml"),
        "[storage]\narchive_after_days=0\n",
    )
    .unwrap();
    age_catalog(&data, &expired.id, "2000-01-01T00:00:00Z");
    let conn = rusqlite::Connection::open(data.join("catalog.db")).unwrap();
    conn.execute(
        "DELETE FROM metadata WHERE key=?1",
        [format!("archived_at:{}", unknown.id)],
    )
    .unwrap();
    drop(conn);
    // Genuine old canonical metadata, not merely a stale catalog projection.
    let conn =
        rusqlite::Connection::open(data.join("chats").join(format!("{}.db", active.id))).unwrap();
    conn.execute("UPDATE sessions SET data=json_set(data,'$.created_at','2000-01-01T00:00:00Z','$.updated_at','2000-01-01T00:00:00Z') WHERE id=?1", [&active.id]).unwrap();
    drop(conn);
    let archived_path = data.join("chats").join(format!("{}.db.zst", expired.id));
    assert!(archived_path.exists());
    // No setting means forever, even for very old archived chats.
    success(cli(root.path(), &data).arg("sessions").output().unwrap());
    assert!(archived_path.exists());
    let output = cli(root.path(), &data)
        .args(["--set", "storage.archived_retention_days=7", "sessions"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(&format!("deleted chat {}", expired.id))
    );
    let listed: Value = serde_json::from_slice(&success(output)).unwrap();
    let ids: Vec<_> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&expired.id.as_str()));
    assert!(!ids.contains(&child.id.as_str()));
    for retained in [&active.id, &recent.id, &unknown.id] {
        assert!(ids.contains(&retained.as_str()));
    }
    assert!(!archived_path.exists());
    assert!(
        !data
            .join("chats")
            .join(format!("{}.db", expired.id))
            .exists()
    );
    let connection = rusqlite::Connection::open(data.join("catalog.db")).unwrap();
    let leaked: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?1)",
            [format!("archived_at:{}", expired.id)],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        !leaked,
        "deletion must remove its archive timestamp projection"
    );
    drop(connection);
    assert!(!data.join("spill").join(&expired.id).exists());
    assert!(!data.join("spill").join(&child.id).exists());
    assert!(
        data.join("chats")
            .join(format!("{}.db.zst", recent.id))
            .exists()
    );
    // Metadata listings with usage must also preserve archived placement.
    success(cli(root.path(), &data).arg("stats").output().unwrap());
    assert!(
        data.join("chats")
            .join(format!("{}.db.zst", recent.id))
            .exists(),
        "stats must not restore archived chats"
    );
    rpc(
        root.path(),
        &data,
        &[json!({"id":1,"method":"session.list"})],
    );
    assert!(
        data.join("chats")
            .join(format!("{}.db.zst", recent.id))
            .exists()
    );
    success(
        cli(root.path(), &data)
            .args(["--set", "storage.archived_retention_days=0", "sessions"])
            .output()
            .unwrap(),
    );
    assert!(
        !data
            .join("chats")
            .join(format!("{}.db.zst", recent.id))
            .exists()
    );
    assert!(
        data.join("chats")
            .join(format!("{}.db", active.id))
            .exists()
    );
    assert!(
        data.join("chats")
            .join(format!("{}.db.zst", unknown.id))
            .exists()
    );
}

#[test]
fn archive_time_survives_catalog_rebuild_and_rearchive_resets_age() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let store = Store::open_data_dir(&data).unwrap();
    let session = store.create_session("archive fixture").unwrap();
    let before = store.storage_stats().unwrap();
    assert!(store.archive_chat(&session.id).unwrap());
    let after = store.storage_stats().unwrap();
    eprintln!(
        "archive metadata fixture: before={} bytes/{} wal/{} tx; after={} bytes/{} wal/{} tx",
        before.database_bytes,
        before.wal_bytes,
        before.transaction_count,
        after.database_bytes,
        after.wal_bytes,
        after.transaction_count
    );
    let archived_at: String = rusqlite::Connection::open(data.join("catalog.db"))
        .unwrap()
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [format!("archived_at:{}", session.id)],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        store
            .archived_roots_before(&archived_at)
            .unwrap()
            .is_empty(),
        "the exact boundary is retained"
    );
    let cutoff = chrono::Utc::now()
        .checked_add_signed(chrono::Duration::seconds(1))
        .unwrap()
        .to_rfc3339();
    assert_eq!(
        store.archived_roots_before(&cutoff).unwrap(),
        vec![session.id.clone()]
    );
    store.catalog_rebuild().unwrap();
    assert_eq!(
        store.archived_roots_before(&cutoff).unwrap(),
        vec![session.id.clone()]
    );
    store.session(&session.id).unwrap();
    assert!(store.archived_roots_before(&cutoff).unwrap().is_empty());
    drop(store);
    age_catalog(&data, &session.id, "2000-01-01T00:00:00Z");
    let store = Store::open_data_dir(&data).unwrap();
    assert!(store.archive_chat(&session.id).unwrap());
    assert!(
        store
            .archived_roots_before("2020-01-01T00:00:00Z")
            .unwrap()
            .is_empty()
    );
}

#[cfg(feature = "web")]
#[tokio::test]
async fn http_create_rename_slash_and_list_preserve_names_without_model_calls() -> anyhow::Result<()>
{
    use bubblepaw_core::{Harness, HarnessConfig, ToolRegistry};
    use bubblepaw_runner::{
        provider::ScriptedProvider,
        web::{WebConfig, WebServices, serve_with_services},
    };
    use std::sync::Arc;
    let root = tempfile::tempdir()?;
    let store = Store::open_data_dir(root.path().join("data"))?;
    let harness = Arc::new(Harness::new(
        store.clone(),
        Arc::new(ScriptedProvider::new("must not run")),
        ToolRegistry::new(),
        HarnessConfig::default(),
    ));
    let assets = root.path().join("dist");
    fs::create_dir(&assets)?;
    fs::write(assets.join("index.html"), "fixture")?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let (stop, receive) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(serve_with_services(
        listener,
        harness,
        store.clone(),
        WebConfig {
            workspace: root.path().display().to_string(),
            model: "scripted".into(),
            provider: "demo".into(),
            read_only: false,
        },
        assets,
        WebServices::default(),
        async {
            let _ = receive.await;
        },
    ));
    let client = reqwest::Client::new();
    let created: Value = client
        .post(format!("{base}/api/sessions"))
        .header("Origin", &base)
        .json(&json!({"name":"HTTP name"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(created["title"], "HTTP name");
    let id = created["id"].as_str().unwrap();
    let renamed: Value = client
        .post(format!("{base}/api/sessions/{id}/rename"))
        .header("Origin", &base)
        .json(&json!({"title":"renamed"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(renamed["title"], "renamed");
    for route in ["run", "steer", "followup"] {
        let value: Value = client
            .post(format!("{base}/api/sessions/{id}/{route}"))
            .header("Origin", &base)
            .json(&json!({"text":format!("/rename {route} name")}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(value["title"], format!("{route} name"));
    }
    let listed: Value = client
        .get(format!("{base}/api/sessions"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(listed[0]["title"], "followup name");
    assert!(store.messages(id)?.is_empty());
    assert!(store.runs(id)?.is_empty());
    stop.send(()).unwrap();
    task.await??;
    Ok(())
}

#[cfg(feature = "web")]
#[path = "session_lifecycle/http_sidebar.rs"]
mod http_sidebar;
