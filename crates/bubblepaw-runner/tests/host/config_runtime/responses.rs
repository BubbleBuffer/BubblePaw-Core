use super::*;

#[tokio::test]
async fn qualified_main_routes_to_configured_responses_provider_without_codex_credentials()
-> Result<()> {
    use axum::{Json, Router, routing::post};
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen = requests.clone();
    let app = Router::new().route("/v1/responses", post(move |Json(body): Json<Value>| {
        let seen = seen.clone();
        async move {
            seen.lock().unwrap().push(body);
            let completed = json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","id":"fixture-message","role":"assistant","content":[{"type":"output_text","text":"fixture complete"}]}],"usage":{"input_tokens":1,"output_tokens":2}}});
            ([("content-type", "text/event-stream")], format!("data: {completed}\n\n"))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}/v1", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let root = tempfile::tempdir()?;
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[model]\nmain='local/fixture'\n[providers.local]\nkind='responses'\nbase_url='{base}'\nkey_env='BUBBLEPAW_TEST_ROLE_KEY'\nreasoning={{reasoning_effort='low'}}\n[context]\nauto_summarize=false\n"
        ),
    )?;
    let mut cli = command(root.path());
    cli.arg("--config")
        .arg(&config)
        .args(["run", "hello"])
        .env("BUBBLEPAW_TEST_ROLE_KEY", "local-fixture-key")
        .env("CODEX_HOME", root.path().join("absent-codex"));
    let output = tokio::process::Command::from(cli).output().await?;
    server.abort();
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["model"], "fixture");
    assert_eq!(requests[0]["reasoning"]["effort"], "low");
    let tools: Vec<_> = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for name in [
        "read",
        "write",
        "edit",
        "list",
        "search",
        "exec",
        "tool_search",
        "call_tool",
    ] {
        assert!(
            tools.contains(&name),
            "native tool {name} missing from {tools:?}"
        );
    }
    assert!(!tools.contains(&"js"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("fixture complete"));
    Ok(())
}
