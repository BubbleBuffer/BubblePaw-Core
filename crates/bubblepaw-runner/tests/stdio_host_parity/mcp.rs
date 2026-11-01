//! MCP servers over the host API: the live server list, an OAuth sign-in
//! completed with a pasted callback URL, `mcp.server` notifications, sign-out,
//! and tokens kept out of `config.get`, the config file and error messages.
use super::*;

#[allow(dead_code)]
#[path = "../../../bubblepaw-mcp/tests/oauth/fake.rs"]
pub(super) mod fake;

impl Host {
    /// Waits for an `mcp.server` notification matching `predicate`.
    async fn mcp_notification(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(found) = self
                    .notifications
                    .iter()
                    .find(|n| n["method"] == "mcp.server" && predicate(&n["params"]))
                {
                    return found["params"].clone();
                }
                let mut line = String::new();
                assert!(self.output.read_line(&mut line).await.unwrap() > 0);
                self.notifications
                    .push(serde_json::from_str(&line).unwrap());
            }
        })
        .await
        .expect("missing mcp.server notification")
    }
}

#[tokio::test]
async fn mcp_sign_in_is_listed_notified_and_keeps_tokens_secret() {
    let fake = fake::Fake::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("config.toml");
    let config = format!(
        "{CONFIG}[mcp.servers.notes]\nurl='{}'\noauth=true\nstartup_timeout_secs=5\ncall_timeout_secs=5\n",
        fake.url()
    );
    std::fs::write(&file, config).unwrap();
    let mut host = Host::start(root.path(), &[]);

    let listed = host.ok("mcp.servers.list", json!({})).await;
    let notes = &listed["servers"][0];
    assert_eq!(notes["name"], "notes");
    assert_eq!(notes["transport"], "http");
    assert_eq!(notes["state"], "error");
    assert_eq!(notes["auth"]["state"], "needs_sign_in");

    let started = host.ok("mcp.auth.start", json!({"server": "notes"})).await;
    let (redirect, state) = fake.approve(started["url"].as_str().unwrap(), "code-1");
    host.mcp_notification(|s| s["auth"]["state"] == "waiting")
        .await;
    let status = host
        .ok(
            "mcp.auth.complete",
            json!({"server": "notes", "redirect_url": format!("{redirect}?code=code-1&state={state}")}),
        )
        .await;
    assert_eq!(status["auth"]["state"], "signed_in");
    assert_eq!(status["state"], "running");
    assert_eq!(status["tool_count"], 2);
    host.mcp_notification(|s| s["auth"]["state"] == "signed_in" && s["state"] == "running")
        .await;
    assert!(root.path().join("data/auth/mcp-notes.json").exists());

    // Tokens never reach config.get, the config file, the list or errors.
    let mut visible = vec![
        host.ok("config.get", json!({})).await.to_string(),
        std::fs::read_to_string(&file).unwrap(),
        host.ok("mcp.servers.list", json!({})).await.to_string(),
        host.call(
            "mcp.auth.complete",
            json!({"server": "notes", "redirect_url": "http://127.0.0.1:1/callback?code=x&state=y"}),
        )
        .await
        .to_string(),
    ];
    visible.push(serde_json::to_string(&host.notifications).unwrap());
    let issued = fake.issued();
    assert_eq!(issued.len(), 2);
    for text in &visible {
        for token in &issued {
            assert!(!text.contains(token.as_str()), "token leaked into {text}");
        }
    }

    let status = host
        .ok("mcp.auth.sign_out", json!({"server": "notes"}))
        .await;
    assert_eq!(status["auth"]["state"], "needs_sign_in");
    assert!(!root.path().join("data/auth/mcp-notes.json").exists());
    let unknown = host
        .call("mcp.auth.start", json!({"server": "other"}))
        .await;
    assert!(
        unknown["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not configured")
    );
    host.stop().await;
}

#[tokio::test]
async fn mcp_header_secrets_are_stored_listed_by_name_and_removed_with_the_server() {
    const VALUE: &str = "synthetic-header-secret-for-this-fixture";
    let fake = fake::Fake::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("config.toml");
    let config = format!(
        "{CONFIG}[mcp.servers.notes]\nurl='{}'\nstartup_timeout_secs=5\ncall_timeout_secs=5\n",
        fake.url()
    );
    std::fs::write(&file, config).unwrap();
    let secrets = root.path().join("data/auth/mcp-notes.json");
    let mut host = Host::start(root.path(), &[]);

    let status = host
        .ok(
            "mcp.headers.set",
            json!({"server": "notes", "header": "X-Api-Key", "value": VALUE}),
        )
        .await;
    assert_eq!(status["name"], "notes");
    assert_eq!(status["headers"], json!(["X-Api-Key"]));
    assert!(std::fs::read_to_string(&secrets).unwrap().contains(VALUE));

    // The list shows the name only; no value reaches config.get, the config
    // file, notifications or any error.
    let mut visible = vec![
        host.ok("config.get", json!({})).await.to_string(),
        std::fs::read_to_string(&file).unwrap(),
        host.ok("mcp.servers.list", json!({})).await.to_string(),
    ];
    for params in [
        json!({"server": "other", "header": "X-Api-Key", "value": VALUE}),
        json!({"server": "notes", "header": "Host", "value": VALUE}),
        json!({"server": "notes", "header": "bad header", "value": VALUE}),
        json!({"server": "notes", "header": "X-Api-Key", "value": 42}),
        json!({"server": "notes", "header": "X-Api-Key", "value": format!("{VALUE}\n")}),
        json!({"server": "notes", "header": "X-Api-Key"}),
    ] {
        let reply = host.call("mcp.headers.set", params).await;
        assert!(reply["error"].is_object(), "{reply}");
        visible.push(reply.to_string());
    }
    visible.push(serde_json::to_string(&host.notifications).unwrap());
    assert!(visible[2].contains("X-Api-Key"));
    for text in &visible {
        assert!(!text.contains(VALUE), "header value leaked into {text}");
    }

    // null removes the value and the emptied secret file.
    let status = host
        .ok(
            "mcp.headers.set",
            json!({"server": "notes", "header": "x-api-key", "value": null}),
        )
        .await;
    assert_eq!(status["headers"], json!([]));
    assert!(!secrets.exists());

    // Removing the server with one config.set deletes its secrets too.
    host.ok(
        "mcp.headers.set",
        json!({"server": "notes", "header": "X-Api-Key", "value": VALUE}),
    )
    .await;
    assert!(secrets.exists());
    host.ok(
        "config.set",
        json!({"key": "mcp.servers.notes", "value": null}),
    )
    .await;
    assert!(!secrets.exists());
    assert_eq!(
        host.ok("mcp.servers.list", json!({})).await["servers"],
        json!([])
    );
    assert!(!std::fs::read_to_string(&file).unwrap().contains("notes"));
    host.stop().await;
}
