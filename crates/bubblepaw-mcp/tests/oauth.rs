//! MCP authorization against a local fake OAuth + MCP server: discovery,
//! registration, PKCE verified by the fake, code exchange over the loopback
//! redirect and as a pasted URL, refresh, refresh failure -> expired, sign-out,
//! status notifications, and tokens kept out of config, status and errors.
use bubblepaw_core::{CancellationToken, Tool, ToolContext};
use bubblepaw_mcp::{
    McpAuthState, McpHost, McpHostOptions, McpServerSettings, McpServerState, McpServerStatus,
    McpSettings,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::broadcast;

#[path = "oauth/fake.rs"]
mod fake;
use fake::Fake;

fn settings(url: &str) -> McpSettings {
    McpSettings {
        servers: BTreeMap::from([(
            "notes".into(),
            McpServerSettings {
                url: Some(url.into()),
                oauth: true,
                startup_timeout_secs: 5,
                call_timeout_secs: 5,
                ..Default::default()
            },
        )]),
        ..Default::default()
    }
}

async fn start(fake: &Fake, root: &Path) -> McpHost {
    let options = McpHostOptions {
        client_env: None,
        auth_dir: Some(root.join("auth")),
    };
    McpHost::start_with_options(&settings(&fake.url()), root, options).await
}

fn notes(host: &McpHost) -> McpServerStatus {
    host.servers()
        .into_iter()
        .find(|s| s.name == "notes")
        .unwrap()
}

async fn eventually(host: &McpHost, mut condition: impl FnMut(&McpServerStatus) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition(&notes(host)) {
        assert!(
            Instant::now() < deadline,
            "status never matched: {:?}",
            notes(host)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Every notification received so far, as JSON.
fn drain(receiver: &mut broadcast::Receiver<McpServerStatus>, seen: &mut Vec<Value>) {
    while let Ok(status) = receiver.try_recv() {
        seen.push(serde_json::to_value(status).unwrap());
    }
}

async fn call(host: &McpHost) -> String {
    let tool: Arc<dyn Tool> = host
        .tools()
        .into_iter()
        .find(|tool| tool.definition().name == "mcp__notes__search")
        .expect("search tool");
    let context = ToolContext {
        session_id: "oauth-session".into(),
        run_id: "oauth-run".into(),
        invocation_id: "oauth-invocation".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    let result = tool.invoke(context, json!({})).await.unwrap();
    serde_json::to_string(&result).unwrap()
}

fn token_file(root: &Path) -> std::path::PathBuf {
    root.join("auth").join("mcp-notes.json")
}

/// Make the stored access token look expired, as time passing would.
fn expire(root: &Path) {
    let path = token_file(root);
    let mut stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    stored["expires_at"] = json!(1);
    std::fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();
}

#[tokio::test]
async fn loopback_sign_in_refreshes_then_expires_and_never_leaks_tokens() {
    let fake = Fake::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let host = start(&fake, root.path()).await;
    let mut receiver = host.subscribe();
    let mut seen = Vec::new();

    // Unauthenticated discovery records the challenge and asks for sign-in.
    let status = notes(&host);
    assert_eq!(status.state, McpServerState::Error);
    assert_eq!(status.auth.state, McpAuthState::NeedsSignIn);
    assert_eq!(status.error.as_deref(), Some("MCP HTTP sign-in required"));

    let authorization = host.auth_start("notes").await.unwrap();
    assert_eq!(notes(&host).auth.state, McpAuthState::Waiting);
    let registration = fake.state.lock().unwrap().registrations[0].clone();
    assert_eq!(registration["token_endpoint_auth_method"], "none");
    let (redirect, state) = fake.approve(&authorization, "code-1");
    assert!(redirect.starts_with("http://127.0.0.1:"), "{redirect}");
    assert_eq!(registration["redirect_uris"], json!([redirect]));

    // The browser follows the redirect; a foreign state is ignored first.
    let browser = reqwest::Client::new();
    let foreign = browser
        .get(format!("{redirect}?code=code-1&state=someone-else"))
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status(), 400);
    let page = browser
        .get(format!("{redirect}?code=code-1&state={state}"))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    eventually(&host, |s| {
        s.state == McpServerState::Running && s.auth.state == McpAuthState::SignedIn
    })
    .await;
    let status = notes(&host);
    assert_eq!(status.tool_count, 2);
    assert_eq!(status.tools.len(), 2);
    assert_eq!(fake.state.lock().unwrap().pkce_failures, 0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(token_file(root.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // Tool results that echo a token are redacted.
    let result = call(&host).await;
    assert!(result.contains("[redacted]"), "{result}");

    // An expiring access token is refreshed before the request.
    expire(root.path());
    let result = call(&host).await;
    assert!(result.contains("[redacted]"), "{result}");
    {
        let fake_state = fake.state.lock().unwrap();
        assert_eq!(fake_state.refresh_grants, 1);
        let current = fake_state.access.clone().unwrap();
        assert!(current.ends_with("-2"));
        assert_eq!(
            fake_state.mcp_authorizations.last().unwrap().as_deref(),
            Some(format!("Bearer {current}").as_str())
        );
        assert!(fake_state.resources.iter().all(|r| r == &fake.url()));
    }

    // A refused refresh moves the server to "sign-in expired".
    fake.state.lock().unwrap().refuse_refresh = true;
    expire(root.path());
    let result = call(&host).await;
    assert!(result.contains("sign-in expired"), "{result}");
    eventually(&host, |s| s.auth.state == McpAuthState::Expired).await;

    drain(&mut receiver, &mut seen);
    let auth_states: Vec<&str> = seen
        .iter()
        .filter(|s| s["name"] == "notes")
        .filter_map(|s| s["auth"]["state"].as_str())
        .collect();
    for expected in ["waiting", "signed_in", "expired"] {
        assert!(auth_states.contains(&expected), "{auth_states:?}");
    }
    assert!(seen.iter().any(|s| s["state"] == "running"), "{seen:?}");

    // Tokens never appear in config, status, notifications or diagnostics.
    let issued = fake.issued();
    assert!(issued.len() >= 4);
    let mut visible = vec![
        serde_json::to_string(&settings(&fake.url())).unwrap(),
        format!("{:?}", settings(&fake.url())),
        serde_json::to_string(&host.servers()).unwrap(),
        serde_json::to_string(&seen).unwrap(),
        host.warnings().join("\n"),
        result,
    ];
    visible.push(
        host.auth_complete("notes", "http://127.0.0.1:1/callback?code=x&state=y")
            .await
            .unwrap_err()
            .to_string(),
    );
    for text in &visible {
        for token in &issued {
            assert!(!text.contains(token.as_str()), "token leaked into {text}");
        }
    }

    // Sign-out forgets the tokens.
    host.auth_sign_out("notes").await.unwrap();
    assert!(!token_file(root.path()).exists());
    eventually(&host, |s| s.auth.state == McpAuthState::NeedsSignIn).await;
    host.shutdown(Duration::ZERO).await;
}

#[tokio::test]
async fn pasted_callback_url_completes_and_pkce_mismatch_fails() {
    let fake = Fake::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let host = start(&fake, root.path()).await;

    // A code minted for a different PKCE challenge is refused by the fake.
    let authorization = host.auth_start("notes").await.unwrap();
    let (redirect, state) = fake.approve(&authorization, "code-wrong");
    fake.state
        .lock()
        .unwrap()
        .codes
        .get_mut("code-wrong")
        .unwrap()
        .0 = "not-the-challenge".into();
    let error = host
        .auth_complete(
            "notes",
            &format!("{redirect}?code=code-wrong&state={state}"),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("invalid_grant"), "{error}");
    assert_eq!(fake.state.lock().unwrap().pkce_failures, 1);
    assert_eq!(notes(&host).auth.state, McpAuthState::Failed);

    // A fresh sign-in completed with the pasted callback URL.
    let authorization = host.auth_start("notes").await.unwrap();
    let (redirect, state) = fake.approve(&authorization, "code-2");
    let foreign = host
        .auth_complete("notes", &format!("{redirect}?code=code-2&state=other"))
        .await
        .unwrap_err();
    assert!(foreign.to_string().contains("does not belong"), "{foreign}");
    assert_eq!(notes(&host).auth.state, McpAuthState::Waiting);
    host.auth_complete("notes", &format!("{redirect}?code=code-2&state={state}"))
        .await
        .unwrap();
    let status = notes(&host);
    assert_eq!(status.auth.state, McpAuthState::SignedIn);
    assert_eq!(status.state, McpServerState::Running);
    assert_eq!(status.tool_count, 2);

    // A second host (another process, a delegate) uses the stored tokens.
    let second = start(&fake, root.path()).await;
    let status = notes(&second);
    assert_eq!(status.state, McpServerState::Running, "{status:?}");
    assert_eq!(status.auth.state, McpAuthState::SignedIn);
    second.shutdown(Duration::ZERO).await;
    host.shutdown(Duration::ZERO).await;
}

#[tokio::test]
async fn oauth_settings_are_validated_and_listed() {
    let mut config = McpServerSettings {
        command: "server".into(),
        oauth: true,
        ..Default::default()
    };
    assert!(
        config
            .validate(&[])
            .unwrap_err()
            .to_string()
            .contains("oauth requires url")
    );
    config.command.clear();
    config.url = Some("https://mcp.example.com/mcp".into());
    config.headers_from = BTreeMap::from([("Authorization".into(), "MCP_TOKEN".into())]);
    assert!(config.validate(&[]).is_err());

    // Without a token folder an OAuth server reports why it cannot start.
    let fake = Fake::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let host = McpHost::start(&settings(&fake.url()), root.path()).await;
    let status = notes(&host);
    assert_eq!(status.state, McpServerState::Error);
    assert!(status.error.unwrap().contains("data directory"));
    assert!(host.auth_start("notes").await.is_err());

    // Disabled servers are listed as stopped.
    let mut disabled = settings(&fake.url());
    disabled.servers.get_mut("notes").unwrap().enabled = false;
    host.reload(&disabled, None).await.unwrap();
    assert_eq!(notes(&host).state, McpServerState::Stopped);
    host.shutdown(Duration::ZERO).await;
}
