use bubblepaw_runner::artifacts::ArtifactStore;
use std::fs;
use uuid::Uuid;

#[test]
fn captures_are_session_owned_immutable_and_survive_restart() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let directory = root.path().join("artifacts");
    fs::create_dir(&workspace).unwrap();
    fs::write(workspace.join("answer.txt"), b"first version").unwrap();
    let session = Uuid::new_v4().to_string();
    let artifact = ArtifactStore::new(&workspace, &directory)
        .unwrap()
        .capture(
            &session,
            "answer.txt",
            Some("report\r\n\";../../a.txt"),
            false,
        )
        .unwrap();
    assert!(!artifact.name.contains(['\r', '\n', '"', ';', '/', '\\']));
    fs::write(workspace.join("answer.txt"), b"changed source").unwrap();
    let store = ArtifactStore::new(&workspace, &directory).unwrap();
    let (saved, bytes) = store.read(&session, &artifact.id).unwrap();
    assert_eq!(saved, artifact);
    assert_eq!(bytes, b"first version");
    assert!(
        store
            .read(&Uuid::new_v4().to_string(), &artifact.id)
            .is_err()
    );
    assert!(store.read(&session, "../../answer").is_err());
    assert!(
        store
            .capture("../session", "answer.txt", None, false)
            .is_err()
    );
}

#[test]
fn rejects_outside_files_directories_oversized_sources_and_active_images() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    fs::write(root.path().join("outside.txt"), b"outside").unwrap();
    fs::write(
        workspace.join("active.svg"),
        b"<svg><script>alert(1)</script></svg>",
    )
    .unwrap();
    let large = fs::File::create(workspace.join("large.bin")).unwrap();
    large.set_len(16 * 1024 * 1024 + 1).unwrap();
    let store = ArtifactStore::new(&workspace, root.path().join("artifacts")).unwrap();
    let session = Uuid::new_v4().to_string();
    assert!(
        store
            .capture(&session, "../outside.txt", None, false)
            .is_err()
    );
    assert!(store.capture(&session, ".", None, false).is_err());
    assert!(store.capture(&session, "large.bin", None, false).is_err());
    assert!(store.capture(&session, "active.svg", None, true).is_err());
    let safe_download = store.capture(&session, "active.svg", None, false).unwrap();
    assert_eq!(safe_download.mime, "application/octet-stream");
}

#[test]
fn reads_revalidate_image_data_and_untrusted_metadata() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("image.png"), b"\x89PNG\r\n\x1a\nfixture").unwrap();
    let directory = root.path().join("artifacts");
    let store = ArtifactStore::new(root.path(), &directory).unwrap();
    let session = Uuid::new_v4().to_string();
    let artifact = store.capture(&session, "image.png", None, true).unwrap();
    assert_eq!(artifact.mime, "image/png");
    let sidecar = directory
        .join(&session)
        .join(format!("{}.json", artifact.id));
    let mut value = serde_json::to_value(&artifact).unwrap();
    value["name"] = serde_json::json!("evil\r\nheader");
    fs::write(&sidecar, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(store.read(&session, &artifact.id).is_err());
    fs::write(&sidecar, serde_json::to_vec(&artifact).unwrap()).unwrap();
    fs::write(
        directory
            .join(&session)
            .join(format!("{}.bin", artifact.id)),
        b"<html>unsafe data</html>",
    )
    .unwrap();
    assert!(store.read(&session, &artifact.id).is_err());
    value = serde_json::to_value(&artifact).unwrap();
    value["path"] = serde_json::json!("../../outside");
    fs::write(&sidecar, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(store.read(&session, &artifact.id).is_err());
    fs::File::create(&sidecar).unwrap().set_len(4097).unwrap();
    assert!(store.read(&session, &artifact.id).is_err());
    fs::write(&sidecar, serde_json::to_vec(&artifact).unwrap()).unwrap();
    fs::File::create(
        directory
            .join(&session)
            .join(format!("{}.bin", artifact.id)),
    )
    .unwrap()
    .set_len(16 * 1024 * 1024 + 1)
    .unwrap();
    assert!(store.read(&session, &artifact.id).is_err());
}

#[cfg(unix)]
#[test]
fn rejects_symlink_escape_from_workspace_and_storage() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    fs::write(root.path().join("outside"), b"outside").unwrap();
    symlink(root.path().join("outside"), workspace.join("escape")).unwrap();
    let directory = root.path().join("artifacts");
    let store = ArtifactStore::new(&workspace, &directory).unwrap();
    let session = Uuid::new_v4().to_string();
    assert!(store.capture(&session, "escape", None, false).is_err());
    symlink(&workspace, directory.join(&session)).unwrap();
    assert!(store.capture(&session, "escape", None, false).is_err());
    assert!(store.read(&session, &Uuid::new_v4().to_string()).is_err());
}
