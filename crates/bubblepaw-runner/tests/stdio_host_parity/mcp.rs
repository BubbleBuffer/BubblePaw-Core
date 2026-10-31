//! MCP servers over the host API: the live server list, an OAuth sign-in
//! completed with a pasted callback URL, `mcp.server` notifications, sign-out,
//! and tokens kept out of `config.get`, the config file and error messages.
use super::*;

#[allow(dead_code)]
#[path = "../../../bubblepaw-mcp/tests/oauth/fake.rs"]
mod fake;

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
