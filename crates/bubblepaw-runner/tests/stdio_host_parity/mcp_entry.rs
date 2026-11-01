//! Whole-entry `config.set` on an MCP server: a disabled start, switching
//! between `command` and `url` with a stored header, and OAuth tokens that
//! follow the entry's `url`.
use super::*;

use super::mcp::fake;

const VALUE: &str = "synthetic-entry-header-secret";

fn secrets(root: &Path) -> String {
    std::fs::read_to_string(root.join("data/auth/mcp-notes.json")).unwrap_or_default()
}

fn entry(url: &str, enabled: bool, oauth: bool) -> Value {
    json!({
        "url": url, "enabled": enabled, "oauth": oauth,
        "startup_timeout_secs": 5, "call_timeout_secs": 5,
    })
}

async fn set(host: &mut Host, value: Value) -> Value {
    host.ok(
        "config.set",
        json!({"key": "mcp.servers.notes", "value": value}),
    )
    .await
}

async fn server(host: &mut Host) -> Value {
    host.ok("mcp.servers.list", json!({})).await["servers"][0].clone()
}

async fn header(host: &mut Host) {
    host.ok(
        "mcp.headers.set",
        json!({"server": "notes", "header": "X-Api-Key", "value": VALUE}),
    )
    .await;
}

fn start(root: &Path) -> Host {
    std::fs::write(root.join("config.toml"), CONFIG).unwrap();
    Host::start(root, &[])
}

#[tokio::test]
async fn a_new_entry_written_disabled_never_connects() {
    let fake = fake::Fake::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let mut host = start(root.path());
    let result = set(&mut host, entry(&fake.url(), false, false)).await;
    assert_eq!(result["applied"], "live");
    let status = server(&mut host).await;
    assert_eq!(status["enabled"], false);
    assert_eq!(status["state"], "stopped");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(fake.state.lock().unwrap().mcp_authorizations.is_empty());
    let text = std::fs::read_to_string(root.path().join("config.toml")).unwrap();
    assert!(text.contains("[mcp.servers.notes]") && text.contains("enabled = false"));
    // Control: turning it on with a leaf write does connect.
    host.ok(
        "config.set",
        json!({"key": "mcp.servers.notes.enabled", "value": true}),
    )
    .await;
    assert!(!fake.state.lock().unwrap().mcp_authorizations.is_empty());
    host.stop().await;
}

#[tokio::test]
async fn switching_between_command_and_url_keeps_the_stored_header() {
    let fake = fake::Fake::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let mut host = start(root.path());
    set(&mut host, entry(&fake.url(), false, false)).await;
    header(&mut host).await;
    assert!(secrets(root.path()).contains(VALUE));
    set(
        &mut host,
        json!({"command": "unused-server-binary", "enabled": false}),
    )
    .await;
    assert_eq!(server(&mut host).await["transport"], "stdio");
    assert!(secrets(root.path()).contains(VALUE));
    set(&mut host, entry(&fake.url(), false, false)).await;
    let status = server(&mut host).await;
    assert_eq!(status["transport"], "http");
    assert_eq!(status["headers"], json!(["X-Api-Key"]));
    assert!(secrets(root.path()).contains(VALUE));
    host.stop().await;
}

#[tokio::test]
async fn a_url_change_drops_the_oauth_tokens_and_an_unchanged_url_keeps_them() {
    let (one, two) = (fake::Fake::spawn().await, fake::Fake::spawn().await);
    let root = tempfile::tempdir().unwrap();
    let mut host = start(root.path());
    set(&mut host, entry(&one.url(), true, true)).await;
    header(&mut host).await;
    let started = host.ok("mcp.auth.start", json!({"server": "notes"})).await;
    let (redirect, state) = one.approve(started["url"].as_str().unwrap(), "code-1");
    host.ok(
        "mcp.auth.complete",
        json!({"server": "notes", "redirect_url": format!("{redirect}?code=code-1&state={state}")}),
    )
    .await;
    assert!(secrets(root.path()).contains("access_token"));
    // The same URL with another field: the tokens stay.
    let mut same = entry(&one.url(), true, true);
    same["call_timeout_secs"] = json!(7);
    set(&mut host, same).await;
    assert!(secrets(root.path()).contains("access_token"));
    // A different URL: the tokens go, the header stays.
    set(&mut host, entry(&two.url(), true, true)).await;
    let text = secrets(root.path());
    assert!(
        !text.contains("access_token"),
        "tokens survived a URL change"
    );
    assert!(text.contains(VALUE));
    host.stop().await;
}

#[tokio::test]
async fn an_invalid_or_misplaced_object_changes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("config.toml");
    let text = format!("{CONFIG}[mcp.servers.notes]\ncommand='x'\nenabled=false\n");
    std::fs::write(&file, text).unwrap();
    let mut host = Host::start(root.path(), &[]);
    let before = std::fs::read(&file).unwrap();
    for (key, value) in [
        (
            "mcp.servers.notes",
            json!({"command": "x", "url": "http://127.0.0.1:1/mcp"}),
        ),
        ("mcp.servers", json!({"notes": {"command": "x"}})),
        ("agents", json!({"auto_wake": true})),
    ] {
        let reply = host
            .call("config.set", json!({"key": key, "value": value}))
            .await;
        assert!(reply["error"].is_object(), "{key}: {reply}");
        assert_eq!(std::fs::read(&file).unwrap(), before, "{key}");
    }
    host.stop().await;
}
