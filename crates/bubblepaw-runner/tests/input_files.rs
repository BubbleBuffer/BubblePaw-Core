use bubblepaw_runner::input_files::{InputFiles, InputLimits};
use serde_json::Value;
use uuid::Uuid;

fn id() -> String {
    Uuid::new_v4().to_string()
}

#[test]
fn record_backed_large_attachments_share_blobs_with_inline_history_and_survive_cache_loss() {
    use bubblepaw_core::{Message, Role, Store};
    let workspace = tempfile::tempdir().unwrap();
    let db = workspace.path().join("record.db");
    let store = Store::open(&db).unwrap();
    let session = store.create_session("attachments").unwrap();
    let storage = InputFiles::new(workspace.path(), InputLimits::default())
        .unwrap()
        .with_store(store.clone());
    let binary = vec![0; 100 * 1024];
    let first = storage.upload(&session.id, "first.bin", &binary).unwrap();
    let second = storage.upload(&session.id, "second.bin", &binary).unwrap();
    let text = "猫\"\\\n".repeat(1200);
    let inline = storage
        .upload(&session.id, "text.txt", text.as_bytes())
        .unwrap();
    let content = storage
        .prepare(
            &session.id,
            "Original prompt",
            std::slice::from_ref(&inline.id),
        )
        .unwrap();
    assert_eq!(files(&content)["files"][0]["inline"], text);
    store
        .append_batch(&[Message {
            id: "attachment-message".into(),
            session_id: session.id.clone(),
            role: Role::User,
            origin: None,
            content: content.clone(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: "2026-10-12T00:00:00Z".into(),
        }])
        .unwrap();
    let connection =
        rusqlite::Connection::open(store.chat_path(&session.id).unwrap().unwrap()).unwrap();
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 3,
        "Two original payloads and one envelope descriptor"
    );
    let raw_bytes: i64 = connection
        .query_row("SELECT SUM(raw_bytes) FROM blobs", [], |r| r.get(0))
        .unwrap();
    assert!(
        raw_bytes < (binary.len() + text.len() + 4096) as i64,
        "Envelope references original text rather than copying its escaped bytes"
    );
    for file in [&first, &second, &inline] {
        std::fs::remove_file(workspace.path().join(&file.path)).unwrap();
        std::fs::remove_file(
            workspace
                .path()
                .join(format!(".run/attachments/{}/{}.json", session.id, file.id)),
        )
        .unwrap();
    }
    assert_eq!(storage.read(&session.id, &first.id).unwrap().1, binary);
    assert_eq!(
        std::fs::read(workspace.path().join(&first.path)).unwrap(),
        binary
    );
    assert!(storage.read(&id(), &first.id).is_err());
    drop(connection);
    drop(storage);
    drop(store);
    let reopened = Store::open(&db).unwrap();
    let storage = InputFiles::new(workspace.path(), InputLimits::default())
        .unwrap()
        .with_store(reopened.clone());
    assert_eq!(
        storage.read(&session.id, &inline.id).unwrap().1,
        text.as_bytes()
    );
    assert_eq!(
        std::fs::read(workspace.path().join(&inline.path)).unwrap(),
        text.as_bytes()
    );
    assert_eq!(reopened.messages(&session.id).unwrap()[0].content, content);
}
fn files(text: &str) -> Value {
    let json = text
        .split("\n\n<BubblePawAttachments>\n")
        .nth(1)
        .unwrap()
        .strip_suffix("\n</BubblePawAttachments>")
        .unwrap();
    serde_json::from_str(json).unwrap()
}

#[test]
fn unicode_prompt_counts_characters_and_plain_input_is_unchanged() {
    let limits = InputLimits::default();
    assert!(limits.validate_prompt(&"🐾".repeat(32_000)).is_ok());
    assert!(limits.validate_prompt(&"🐾".repeat(32_001)).is_err());
    assert!(limits.validate_prompt("  ").is_err());
    let workspace = tempfile::tempdir().unwrap();
    let storage = InputFiles::new(workspace.path(), limits).unwrap();
    let plain = "Literal <BubblePawAttachments> not trusted </BubblePawAttachments>";
    assert_eq!(storage.prepare(&id(), plain, &[]).unwrap(), plain);
}

#[test]
fn ownership_ids_count_duplicates_and_combined_budget_precede_admission() {
    let workspace = tempfile::tempdir().unwrap();
    let storage = InputFiles::new(
        workspace.path(),
        InputLimits {
            max_file_bytes: 10,
            max_total_bytes: 12,
            max_files: 2,
            ..Default::default()
        },
    )
    .unwrap();
    let session = id();
    let a = storage.upload(&session, "one.txt", b"1234567").unwrap();
    let b = storage.upload(&session, "two.txt", b"abcdefg").unwrap();
    assert!(storage.upload(&session, "large", b"12345678901").is_err());
    assert!(
        storage
            .prepare(&session, "test", &[a.id.clone(), b.id])
            .is_err()
    );
    assert!(
        storage
            .prepare(&session, "", &[a.id.clone(), a.id.clone()])
            .is_err()
    );
    assert!(
        storage
            .prepare(&session, "", &[a.id.clone(), id(), id()])
            .is_err()
    );
    assert!(
        storage
            .prepare(&id(), "", std::slice::from_ref(&a.id))
            .is_err()
    );
    assert!(storage.read(&session, "../outside").is_err());
    assert!(storage.read(&session, &a.id.to_uppercase()).is_err());
    let message = storage.prepare(&session, "", &[a.id]).unwrap();
    assert!(message.starts_with("Attached files.\n"));
    assert_eq!(files(&message)["files"][0]["inline"], "1234567");
}

#[test]
fn entire_envelope_is_bounded_and_omission_never_truncates_originals() {
    let workspace = tempfile::tempdir().unwrap();
    let storage = InputFiles::new(workspace.path(), InputLimits::default()).unwrap();
    let session = id();
    let first = storage
        .upload(&session, "first.txt", &b"abc ".repeat(120))
        .unwrap();
    let second = storage
        .upload(&session, "second.txt", &b"def ".repeat(120))
        .unwrap();
    let full = storage
        .prepare(
            &session,
            "Original\r\nprompt",
            &[first.id.clone(), second.id.clone()],
        )
        .unwrap();
    let limits = InputLimits {
        inline_tokens: tiktoken_rs::o200k_base_singleton()
            .encode_ordinary(&full["Original\r\nprompt".len()..])
            .len()
            - 1,
        ..Default::default()
    };
    let storage = InputFiles::new(workspace.path(), limits).unwrap();
    let message = storage
        .prepare(
            &session,
            "Original\r\nprompt",
            &[first.id.clone(), second.id.clone()],
        )
        .unwrap();
    assert!(message.starts_with("Original\r\nprompt\n\n<BubblePawAttachments>"));
    let envelope = &message["Original\r\nprompt".len()..];
    assert!(
        tiktoken_rs::o200k_base_singleton()
            .encode_ordinary(envelope)
            .len()
            <= limits.inline_tokens
    );
    let refs = files(&message);
    assert!(
        refs["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["omitted"] == true)
    );
    assert_eq!(
        storage.read(&session, &first.id).unwrap().1,
        b"abc ".repeat(120)
    );
    assert_eq!(
        storage.read(&session, &second.id).unwrap().1,
        b"def ".repeat(120)
    );
}

#[test]
fn large_full_file_and_binary_survive_reopening_with_safe_names() {
    let workspace = tempfile::tempdir().unwrap();
    let session = id();
    let bytes = b"full original text\n".repeat(600_000);
    let storage = InputFiles::new(workspace.path(), InputLimits::default()).unwrap();
    let text = storage.upload(&session, "../CON.txt", &bytes).unwrap();
    let binary = storage
        .upload(&session, "evil\r\nheader:\".bin", &[0, 255, 128, 1])
        .unwrap();
    assert!(!text.name.contains('/') && !binary.name.contains('\r') && !binary.name.contains('"'));
    assert!(workspace.path().join(&text.path).is_file());
    let restarted = InputFiles::new(workspace.path(), InputLimits::default()).unwrap();
    let message = restarted
        .prepare(&session, "inspect", &[text.id.clone(), binary.id.clone()])
        .unwrap();
    let refs = files(&message);
    assert_eq!(refs["files"][0]["omitted"], true);
    assert!(refs["files"][0].get("inline").is_none());
    assert_eq!(refs["files"][1]["text"], false);
    assert_eq!(restarted.read(&session, &text.id).unwrap().1, bytes);
    assert_eq!(
        restarted.read(&session, &binary.id).unwrap().1,
        vec![0, 255, 128, 1]
    );
}

#[cfg(unix)]
#[test]
fn redirected_file_and_session_directory_are_rejected() {
    use std::os::unix::fs::symlink;
    let workspace = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let session = id();
    let storage = InputFiles::new(workspace.path(), InputLimits::default()).unwrap();
    let file = storage.upload(&session, "fixture", b"original").unwrap();
    let path = workspace.path().join(&file.path);
    std::fs::remove_file(&path).unwrap();
    std::fs::write(external.path().join("fixture"), "external").unwrap();
    symlink(external.path().join("fixture"), &path).unwrap();
    assert!(storage.read(&session, &file.id).is_err());
    let foreign = id();
    symlink(
        external.path(),
        workspace.path().join(".run/attachments").join(&foreign),
    )
    .unwrap();
    assert!(storage.upload(&foreign, "fixture", b"data").is_err());
}

#[test]
#[cfg_attr(windows, allow(clippy::permissions_set_readonly_false))]
fn changed_blob_fails_integrity_check_instead_of_sending_new_bytes() {
    let workspace = tempfile::tempdir().unwrap();
    let session = id();
    let storage = InputFiles::new(workspace.path(), InputLimits::default()).unwrap();
    let file = storage.upload(&session, "fixture", b"original").unwrap();
    let path = workspace.path().join(&file.path);
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    #[cfg(windows)]
    permissions.set_readonly(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o600);
    }
    std::fs::set_permissions(&path, permissions).unwrap();
    std::fs::write(&path, b"modified").unwrap();
    assert!(storage.read(&session, &file.id).is_err());
    assert!(storage.prepare(&session, "inspect", &[file.id]).is_err());
}

#[cfg(windows)]
#[test]
fn windows_junction_cannot_redirect_session_storage_outside_workspace() {
    let workspace = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let storage = InputFiles::new(workspace.path(), InputLimits::default()).unwrap();
    let session = id();
    let link = workspace.path().join(".run/attachments").join(&session);
    let result = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:BUBBLEPAW_TEST_LINK -Target $env:BUBBLEPAW_TEST_TARGET | Out-Null"])
        .env("BUBBLEPAW_TEST_LINK", &link).env("BUBBLEPAW_TEST_TARGET", external.path()).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(storage.upload(&session, "fixture", b"data").is_err());
    assert!(storage.read(&session, &id()).is_err());
    assert!(std::fs::read_dir(external.path()).unwrap().next().is_none());
}
