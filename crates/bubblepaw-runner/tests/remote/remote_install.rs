//! `remote tunnel install` against a local fake GitHub release: a matching
//! checksum installs the program; a wrong one is refused and leaves nothing.
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use bubblepaw_runner::remote::install::{self, InstallSource};
use sha2::{Digest, Sha256};
use std::sync::Arc;

const BODY: &[u8] = b"#!/bin/sh\necho fake cloudflared\n";

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Serve a release whose notes list `listed` as the asset's checksum.
async fn fake_release(listed: String) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let asset = install::asset_name().unwrap();
    let release = serde_json::json!({
        "tag_name": "2026.10.1",
        "body": format!("### SHA256 Checksums:\n```\n{asset}: {listed}\n```\n"),
        "assets": [{
            "name": asset,
            "browser_download_url": format!("{base}/download/{asset}"),
        }],
    });
    let app = Router::new()
        .route(
            "/releases/tags/2026.10.1",
            get(|State(release): State<Arc<serde_json::Value>>| async move {
                axum::Json((*release).clone())
            }),
        )
        .route(&format!("/download/{asset}"), get(|| async { BODY }))
        .with_state(Arc::new(release));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("{base}/releases/tags")
}

fn source(api: String) -> InstallSource {
    InstallSource {
        api,
        version: "2026.10.1".into(),
    }
}

#[tokio::test]
async fn a_verified_download_is_installed() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("remote/cloudflare/bin");
    let api = fake_release(sha256(BODY)).await;
    let installed = install::install(&bin, &source(api)).await.unwrap();
    assert_eq!(installed.parent(), Some(bin.as_path()));
    assert_eq!(std::fs::read(&installed).unwrap(), BODY);
    // Only the program: no temporary files left next to it.
    assert_eq!(std::fs::read_dir(&bin).unwrap().count(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&installed).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }
}

#[tokio::test]
async fn a_wrong_checksum_is_refused_and_leaves_nothing() {
    let root = tempfile::tempdir().unwrap();
    let cloudflare = root.path().join("remote/cloudflare");
    let bin = cloudflare.join("bin");
    let api = fake_release(sha256(b"something else")).await;
    let error = install::install(&bin, &source(api)).await.unwrap_err();
    assert!(format!("{error:#}").contains("SHA-256"), "{error:#}");
    assert!(!bin.exists());
    assert!(!cloudflare.exists());
    assert!(!root.path().join("remote").exists());
}

#[tokio::test]
async fn a_missing_release_is_an_error() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    let api = fake_release(sha256(BODY)).await;
    let mut wrong = source(api);
    wrong.version = "1999.1.1".into();
    assert!(install::install(&bin, &wrong).await.is_err());
    assert!(!bin.exists());
}
