//! The Cloudflare status calls an app's Remote access page makes, against a
//! fake `cloudflared` and a fake Cloudflare API: login state, `tunnel_login`,
//! account and zones (by API token and by `cert.pem`), the installed version
//! and the checksum-verified update. No real Cloudflare is contacted.
#[allow(dead_code)]
#[path = "remote_tunnel/fake.rs"]
mod fake;
#[allow(dead_code)]
#[path = "remote_tunnel/fake_api.rs"]
mod fake_api;

use axum::Router;
use axum::extract::State;
use axum::routing::get;
use base64::Engine;
use bubblepaw_runner::remote::install::{self, InstallSource};
use bubblepaw_runner::remote::{
    CLOUDFLARED_VERSION, RemotePaths, TokenSource, cloudflare_account, cloudflare_login_state,
    cloudflared_info, forget_cloudflare_login, forget_cloudflare_token, store_cloudflare_token,
    tunnel_login, update_cloudflared,
};
use fake_api::{FakeApi, TOKEN};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Add `extra`'s keys to the sidecar that scripts the fake in `dir`.
fn script(dir: &Path, extra: serde_json::Value) {
    let path = dir.join("fake-cloudflared.json");
    let mut sidecar: serde_json::Value = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    for (key, value) in extra.as_object().unwrap() {
        sidecar[key] = value.clone();
    }
    std::fs::write(path, sidecar.to_string()).unwrap();
}

/// A `cert.pem` like `cloudflared tunnel login` writes, naming `zone-1`.
fn cert_pem() -> String {
    let login = serde_json::json!({"zoneID": "zone-1", "accountID": "acct-1", "apiToken": TOKEN});
    let body = base64::engine::general_purpose::STANDARD.encode(login.to_string());
    format!("-----BEGIN ARGO TUNNEL TOKEN-----\n{body}\n-----END ARGO TUNNEL TOKEN-----\n")
}

/// These tests need `BUBBLEPAW_CLOUDFLARE_TOKEN` unset.
fn paths(root: &Path) -> RemotePaths {
    RemotePaths::in_data_dir(&root.join("data"))
}

#[test]
fn signing_out_removes_only_the_browser_certificate() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    std::fs::create_dir_all(&paths.cloudflare).unwrap();
    let cert = paths.cloudflare.join("cert.pem");
    std::fs::write(&cert, cert_pem()).unwrap();
    for name in ["tunnel.json", "state.json", "api-token"] {
        std::fs::write(paths.cloudflare.join(name), format!("preserve-{name}")).unwrap();
    }
    assert!(forget_cloudflare_login(Some(&cert)).unwrap());
    assert!(!forget_cloudflare_login(Some(&cert)).unwrap());
    for name in ["tunnel.json", "state.json", "api-token"] {
        assert_eq!(
            std::fs::read_to_string(paths.cloudflare.join(name)).unwrap(),
            format!("preserve-{name}")
        );
    }
}

#[tokio::test]
async fn executable_validation_rejects_missing_non_program_non_cloudflared_and_timeout() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    assert!(
        cloudflared_info(&root.path().join("missing"), &paths)
            .await
            .is_err()
    );
    assert!(cloudflared_info(root.path(), &paths).await.is_err());
    let text = root.path().join("plain-file");
    std::fs::write(&text, "not a program").unwrap();
    assert!(cloudflared_info(&text, &paths).await.is_err());
    let fake = fake::install(&root.path().join("bin"), &[], None);
    script(
        fake.program.parent().unwrap(),
        serde_json::json!({"version_text":"another program version 2026.9.0"}),
    );
    assert!(
        cloudflared_info(&fake.program, &paths)
            .await
            .unwrap_err()
            .to_string()
            .contains("identify")
    );
    script(
        fake.program.parent().unwrap(),
        serde_json::json!({"version_text":"cloudflared version 2026.9.0", "version_exit_code":2}),
    );
    assert!(
        cloudflared_info(&fake.program, &paths)
            .await
            .unwrap_err()
            .to_string()
            .contains("failed")
    );
    script(
        fake.program.parent().unwrap(),
        serde_json::json!({"version_exit_code":0,"version_delay_ms":10000}),
    );
    let started = std::time::Instant::now();
    assert!(
        cloudflared_info(&fake.program, &paths)
            .await
            .unwrap_err()
            .to_string()
            .contains("in time")
    );
    assert!(started.elapsed() < Duration::from_secs(8));
}

#[tokio::test]
async fn login_state_follows_the_certificate_and_the_stored_token() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    let cert = root.path().join("cert.pem");
    let state = cloudflare_login_state(&paths, Some(&cert));
    assert!(!state.signed_in());
    std::fs::write(&cert, cert_pem()).unwrap();
    let state = cloudflare_login_state(&paths, Some(&cert));
    assert!(state.cert && state.token.is_none() && state.signed_in());
    store_cloudflare_token(&paths, TOKEN).unwrap();
    let state = cloudflare_login_state(&paths, Some(&cert));
    assert_eq!(state.token, Some(TokenSource::File));
    assert!(store_cloudflare_token(&paths, "two words").is_err());
    assert!(forget_cloudflare_token(&paths).unwrap());
    assert!(!forget_cloudflare_token(&paths).unwrap());
    assert_eq!(cloudflare_login_state(&paths, Some(&cert)).token, None);
}

#[tokio::test]
async fn tunnel_login_returns_the_url_and_waits_for_the_certificate() {
    let root = tempfile::tempdir().unwrap();
    let cert = root.path().join("login").join("cert.pem");
    std::fs::create_dir_all(cert.parent().unwrap()).unwrap();
    let fake = fake::install(&root.path().join("bin"), &[], None);
    script(
        fake.program.parent().unwrap(),
        serde_json::json!({
            "login_url": "https://login.example.test/one-time-link",
            "cert": {"path": cert, "contents": cert_pem()},
        }),
    );
    let login = tunnel_login(&fake.program, Some(&cert)).await.unwrap();
    assert_eq!(login.url(), "https://login.example.test/one-time-link");
    login.wait(Duration::from_secs(20)).await.unwrap();
    assert!(cloudflare_login_state(&paths(root.path()), Some(&cert)).cert);
}

#[tokio::test]
async fn account_and_zones_come_from_a_token_or_from_the_certificate() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    let cert = root.path().join("cert.pem");
    let api = FakeApi::start(Vec::new()).await;
    // Nobody is signed in.
    assert!(
        cloudflare_account(&paths, Some(&cert), Some(&api.url))
            .await
            .unwrap()
            .is_none()
    );
    // The certificate names one zone.
    std::fs::write(&cert, cert_pem()).unwrap();
    let account = cloudflare_account(&paths, Some(&cert), Some(&api.url))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.name.as_deref(), Some("Acme Inc"));
    assert_eq!(account.zones.len(), 1);
    assert_eq!(account.zones[0].name, "example.com");
    // An API token lists every zone it sees.
    store_cloudflare_token(&paths, TOKEN).unwrap();
    let account = cloudflare_account(&paths, Some(&cert), Some(&api.url))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.name.as_deref(), Some("Acme Inc"));
    let names: Vec<&str> = account
        .zones
        .iter()
        .map(|zone| zone.name.as_str())
        .collect();
    assert_eq!(names, ["example.com", "other.dev"]);
    // A token the API refuses is an error, not an empty account.
    store_cloudflare_token(&paths, "wrong-token-not-real").unwrap();
    assert!(
        cloudflare_account(&paths, Some(&cert), Some(&api.url))
            .await
            .is_err()
    );
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A fake GitHub release for `version` whose download is `body`, listing
/// `listed` as the checksum.
async fn fake_release(version: &str, body: Vec<u8>, listed: String) -> InstallSource {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let asset = install::asset_name().unwrap();
    let release = serde_json::json!({
        "tag_name": version,
        "body": format!("### SHA256 Checksums:\n```\n{asset}: {listed}\n```\n"),
        "assets": [{
            "name": asset,
            "browser_download_url": format!("{base}/download/{asset}"),
        }],
    });
    let body = Arc::new(body);
    let app = Router::new()
        .route(
            &format!("/releases/tags/{version}"),
            get(|State(release): State<Arc<serde_json::Value>>| async move {
                axum::Json((*release).clone())
            }),
        )
        .route(
            &format!("/download/{asset}"),
            get(move || {
                let body = body.clone();
                async move { (*body).clone() }
            }),
        )
        .with_state(Arc::new(release));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    InstallSource {
        api: format!("{base}/releases/tags"),
        version: version.to_owned(),
    }
}

#[tokio::test]
async fn an_old_cloudflared_reports_an_update_and_the_update_installs_the_pinned_one() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    // An older copy from elsewhere: version known, update offered, not managed.
    let old = fake::install(&root.path().join("elsewhere"), &[], None);
    script(
        old.program.parent().unwrap(),
        serde_json::json!({"version": "2026.9.0"}),
    );
    let info = cloudflared_info(&old.program, &paths).await.unwrap();
    assert_eq!(info.version.as_deref(), Some("2026.9.0"));
    assert_eq!(info.pinned, CLOUDFLARED_VERSION);
    assert!(info.update_available);
    assert!(!info.managed);

    // A release whose checksum does not match installs nothing.
    let body = std::fs::read(env!("CARGO_BIN_EXE_bubblepaw-fake-cloudflared")).unwrap();
    let bad = fake_release(CLOUDFLARED_VERSION, body.clone(), sha256(b"other bytes")).await;
    assert!(update_cloudflared(&paths, &bad).await.is_err());
    assert!(!paths.cloudflare_bin().exists());

    // The update installs a program that says it is the pinned version.
    let listed = sha256(&body);
    let source = fake_release(CLOUDFLARED_VERSION, body, listed).await;
    let bin = paths.cloudflare_bin();
    std::fs::create_dir_all(&bin).unwrap();
    script(&bin, serde_json::json!({"version": CLOUDFLARED_VERSION}));
    let info = update_cloudflared(&paths, &source).await.unwrap();
    assert_eq!(info.version.as_deref(), Some(CLOUDFLARED_VERSION));
    assert!(!info.update_available);
    assert!(info.managed);
}
