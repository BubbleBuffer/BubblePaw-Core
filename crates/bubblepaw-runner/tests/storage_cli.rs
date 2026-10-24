//! Offline storage commands exercise the real binary without provider credentials.
use bubblepaw_core::{ContextStrategy, Message, Role, Store};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture {
    root: tempfile::TempDir,
    data: PathBuf,
    parent: String,
    child: String,
    payload: String,
    attachment_hash: String,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        fs::write(
            root.path().join("config.toml"),
            "[providers.codex]\nauth_file = 'missing-credentials.json'\n",
        )
        .unwrap();
        let store = Store::open_data_dir(&data).unwrap();
        let parent = store.create_session("root chat").unwrap();
        let child = store
            .create_session_with_parent("child", ContextStrategy::Lcm, Some(&parent.id))
            .unwrap();
        let payload = "large blob output with newline\n".repeat(400);
        let attachment_hash = store.store_attachment(
            &parent.id,
            "synthetic-binary",
            r#"{"filename":"synthetic.bin","size":6000,"media_type":"application/octet-stream"}"#,
            &[0, 1, 255].repeat(2000),
        ).unwrap();
        store
            .append_batch(&[
                message(
                    &parent.id,
                    Role::User,
                    "one complete message\nwith a newline",
                ),
                message(&parent.id, Role::Tool, &payload),
                message(&child.id, Role::Assistant, "independent child reply"),
            ])
            .unwrap();
        drop(store);
        Self {
            root,
            data,
            parent: parent.id,
            child: child.id,
            payload,
            attachment_hash,
        }
    }

    fn cli(&self) -> Command {
        let mut command = self.base_cli();
        command.arg("--data-dir").arg(&self.data);
        command
    }

    fn base_cli(&self) -> Command {
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
            .arg(self.root.path());
        command
    }
}

fn message(session: &str, role: Role, text: &str) -> Message {
    Message {
        id: uuid::Uuid::new_v4().to_string(),
        session_id: session.into(),
        role,
        origin: None,
        content: text.into(),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: "2026-10-12T12:00:00Z".into(),
    }
}

fn success(output: Output) -> Vec<u8> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, (usize, String)> {
    use sha2::{Digest, Sha256};
    fn visit(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, (usize, String)>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, result);
            } else if path.extension().is_some_and(|extension| extension == "db") {
                let bytes = fs::read(&path).unwrap();
                result.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    (bytes.len(), format!("{:x}", Sha256::digest(&bytes))),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

#[test]
fn export_is_read_only_message_jsonl_with_inline_blobs_and_child_links() {
    let fixture = Fixture::new();
    let before = snapshot(&fixture.data);
    let output = success(
        fixture
            .cli()
            .args(["export", &fixture.parent])
            .output()
            .unwrap(),
    );
    let text = String::from_utf8(output).unwrap();
    let records: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        records.len(),
        2,
        "export includes only the selected session's messages"
    );
    assert!(
        records
            .iter()
            .any(|record| record["message"]["content"] == fixture.payload)
    );
    assert!(
        records
            .iter()
            .all(|record| record["session_id"] == fixture.parent)
    );
    assert!(records.iter().any(|record| {
        record["child_session_ids"]
            .as_array()
            .is_some_and(|ids| ids.contains(&Value::String(fixture.child.clone())))
    }));
    let attachments = records[0]["attachments"]
        .as_array()
        .expect("original attachment blobs inline on the first message");
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0]["id"], "synthetic-binary");
    assert_eq!(attachments[0]["blob_hash"], fixture.attachment_hash);
    assert_eq!(attachments[0]["encoding"], "base64");
    assert_eq!(attachments[0]["data"], "AAH/".repeat(2000));
    assert_eq!(attachments[0]["metadata"]["filename"], "synthetic.bin");
    assert!(records.iter().skip(1).all(|record| {
        record
            .get("attachments")
            .is_none_or(|value| value.as_array().is_some_and(Vec::is_empty))
    }));
    assert_eq!(before, snapshot(&fixture.data));
    assert!(!fixture.root.path().join("codex").exists());
    let child = success(
        fixture
            .cli()
            .args(["export", &fixture.child])
            .output()
            .unwrap(),
    );
    let child: Value = serde_json::from_slice(&child).unwrap();
    assert_eq!(child["session_id"], fixture.child);
    assert_eq!(child["parent_session_id"], fixture.parent);
    assert_eq!(child["root_id"], fixture.parent);
    assert_eq!(child["message"]["content"], "independent child reply");
    assert_eq!(before, snapshot(&fixture.data));
    let out = fixture.root.path().join("history.jsonl");
    success(
        fixture
            .cli()
            .args(["export", &fixture.parent, "--out"])
            .arg(&out)
            .output()
            .unwrap(),
    );
    assert_eq!(fs::read(&out).unwrap(), text.as_bytes());
    let second = fixture
        .cli()
        .args(["export", &fixture.parent, "--out"])
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        !second.status.success(),
        "existing output files are never overwritten"
    );
}

#[test]
fn catalog_rebuild_serving_wipe_and_tree_delete_need_no_credentials() {
    let fixture = Fixture::new();
    let chat = fixture
        .data
        .join("chats")
        .join(format!("{}.db", fixture.parent));
    let original = success(
        fixture
            .cli()
            .args(["history", &fixture.parent])
            .output()
            .unwrap(),
    );
    fs::remove_file(fixture.data.join("catalog.db")).unwrap();
    let rebuilt: Value = serde_json::from_slice(&success(
        fixture.cli().args(["catalog", "rebuild"]).output().unwrap(),
    ))
    .unwrap();
    assert_eq!(rebuilt["sessions"], 2);
    success(fixture.cli().args(["serving", "wipe"]).output().unwrap());
    assert_eq!(
        success(
            fixture
                .cli()
                .args(["history", &fixture.parent])
                .output()
                .unwrap()
        ),
        original,
        "maintenance preserved canonical history"
    );
    let history: Value = serde_json::from_slice(&success(
        fixture
            .cli()
            .args(["history", &fixture.child])
            .output()
            .unwrap(),
    ))
    .unwrap();
    assert_eq!(history[0]["content"], "independent child reply");
    success(
        fixture
            .cli()
            .args(["delete", &fixture.child])
            .output()
            .unwrap(),
    );
    assert!(chat.exists());
    let sessions: Vec<Value> =
        serde_json::from_slice(&success(fixture.cli().arg("sessions").output().unwrap())).unwrap();
    assert_eq!(sessions.len(), 1);
    success(
        fixture
            .cli()
            .args(["delete", &fixture.parent])
            .output()
            .unwrap(),
    );
    assert!(!chat.exists());
    let sessions: Vec<Value> =
        serde_json::from_slice(&success(fixture.cli().arg("sessions").output().unwrap())).unwrap();
    assert!(sessions.is_empty());
    assert!(!fixture.root.path().join("codex").exists());
}

#[test]
fn export_unknown_session_does_not_create_a_layout() {
    let fixture = Fixture::new();
    let absent = fixture.root.path().join("absent");
    let output = fixture
        .cli()
        .arg("--set")
        .arg(format!("storage.data_dir={}", absent.display()))
        .args(["export", "missing"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!absent.exists());
}

#[test]
fn import_archive_restore_and_search_roundtrip_are_offline() {
    let fixture = Fixture::new();
    let source = fixture.root.path().join("claude.jsonl");
    let records = [
        serde_json::json!({"type":"user","message":{"content":"Find the synthetic needle."}}),
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":"The needle is retained."}]}}),
        serde_json::json!({"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"auto"}}),
        serde_json::json!({"type":"user","isCompactSummary":true,"message":{"content":"Source summary about the needle."}}),
    ];
    fs::write(
        &source,
        records
            .iter()
            .map(|record| format!("{record}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let result: Value = serde_json::from_slice(&success(
        fixture
            .cli()
            .args(["import", "claude-code"])
            .arg(&source)
            .args(["--title", "Imported synthetic"])
            .output()
            .unwrap(),
    ))
    .unwrap();
    let id = result["root_session_id"].as_str().unwrap();
    assert_eq!(result["stats"]["messages"], 2);
    assert_eq!(result["stats"]["compactions"], 1);
    let chat = fixture.data.join("chats").join(format!("{id}.db"));
    let archived = chat.with_extension("db.zst");
    let search = || -> Value {
        serde_json::from_slice(&success(
            fixture
                .cli()
                .args(["search", "needle", "--chat", id])
                .output()
                .unwrap(),
        ))
        .unwrap()
    };
    let before = search();
    assert_eq!(before.as_array().unwrap().len(), 2);
    let history = success(fixture.cli().args(["export", id]).output().unwrap());
    assert!(String::from_utf8_lossy(&history).contains("import_derived"));
    success(fixture.cli().args(["archive", id]).output().unwrap());
    assert!(archived.exists() && !chat.exists());
    assert_eq!(
        history,
        success(fixture.cli().args(["export", id]).output().unwrap())
    );
    assert!(archived.exists() && !chat.exists());
    success(fixture.cli().args(["unarchive", id]).output().unwrap());
    assert!(chat.exists() && !archived.exists());
    assert_eq!(before, search());
    success(fixture.cli().args(["serving", "wipe"]).output().unwrap());
    assert_eq!(before, search());
    success(fixture.cli().args(["delete", id]).output().unwrap());
    assert!(search().as_array().unwrap().is_empty());
}

#[test]
fn legacy_db_alias_and_configured_archive_migrate_once_preserving_source() {
    let fixture = Fixture::new();
    let legacy = fixture.root.path().join("legacy.sqlite");
    // Use the frozen genuine pre-split schema, with inline legacy messages.
    let connection = rusqlite::Connection::open(&legacy).unwrap();
    connection
        .execute_batch(include_str!(
            "../../bubblepaw-core/tests/fixtures/main-v6-schema.sql"
        ))
        .unwrap();
    let store = Store::open_data_dir(&fixture.data).unwrap();
    for session in store.list_sessions().unwrap() {
        connection
            .execute(
                "INSERT INTO sessions(id,data) VALUES(?1,?2)",
                rusqlite::params![session.id, serde_json::to_string(&session).unwrap()],
            )
            .unwrap();
        for message in store.messages(&session.id).unwrap() {
            connection
                .execute(
                    "INSERT INTO messages(session_id,data) VALUES(?1,?2)",
                    rusqlite::params![session.id, serde_json::to_string(&message).unwrap()],
                )
                .unwrap();
        }
    }
    drop(store);
    drop(connection);
    let source = fs::read(&legacy).unwrap();
    let legacy_run = || {
        fixture
            .base_cli()
            .arg("--db")
            .arg(&legacy)
            .arg("sessions")
            .output()
            .unwrap()
    };
    let output = legacy_run();
    assert!(String::from_utf8_lossy(&output.stderr).contains("--db is a legacy alias"));
    let sessions: Vec<Value> = serde_json::from_slice(&success(output)).unwrap();
    assert_eq!(sessions.len(), 2);
    assert!(
        fs::read(&legacy).unwrap() == source,
        "migration modified original legacy file"
    );
    assert!(Store::legacy_data_dir(&legacy).join("catalog.db").exists());
    success(legacy_run());
    assert!(
        fs::read(&legacy).unwrap() == source,
        "repeated startup modified original legacy file"
    );

    let configured = fixture.root.path().join("configured-data");
    fs::create_dir(&configured).unwrap();
    let old_archive = configured.join("archive.sqlite");
    fs::write(&old_archive, &source).unwrap();
    let sessions: Vec<Value> = serde_json::from_slice(&success(
        fixture
            .cli()
            .arg("--set")
            .arg(format!("storage.data_dir={}", configured.display()))
            .arg("sessions")
            .output()
            .unwrap(),
    ))
    .unwrap();
    assert_eq!(sessions.len(), 2);
    assert!(
        fs::read(&old_archive).unwrap() == source,
        "default archive migration modified original file"
    );
    assert!(configured.join("catalog.db").exists());
}
