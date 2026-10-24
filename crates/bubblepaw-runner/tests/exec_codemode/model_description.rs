use super::*;

#[tokio::test]
async fn js_tool_description_the_model_sees_names_sync_background_steer_and_poll_contract() {
    let f = Fixture::new(5);
    // JS carries composition guidance and concise always-shown signatures.
    // Full tool contracts remain available through model-visible discovery.
    let js = f
        .registry
        .get("js")
        .unwrap()
        .definition_in(&f.registry)
        .description
        .to_lowercase();
    for phrase in [
        "background:true",
        "wait({handle})",
        "tools.jobs({",
        "tools.poll({",
    ] {
        assert!(js.contains(phrase), "js reference lacks {phrase:?}");
    }
    // `cancel` is not always shown in JS help, so it is listed by name only.
    let other_tools = js
        .lines()
        .find(|line| line.starts_with("other tools"))
        .expect("js reference lists deferred tools");
    assert!(
        other_tools
            .split(": ")
            .nth(1)
            .unwrap_or_default()
            .split(", ")
            .any(|name| name.trim() == "cancel"),
        "deferred tools lack cancel: {other_tools:?}"
    );
    let discovered = f
        .harness
        .call_tool(
            &f.session,
            "js",
            json!({"code":"return await tools.$search('select:exec,poll,wait,jobs,cancel');"}),
            RunOptions::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!discovered.is_error, "{}", discovered.content);
    let discovered: Value = serde_json::from_str(&discovered.content).unwrap();
    let definitions = discovered["tools"].as_array().unwrap();
    assert_eq!(definitions.len(), 5);
    let description = |name| {
        definitions
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap()["description"]
            .as_str()
            .unwrap()
    };
    for phrase in [
        "Sync by default, up to exec.sync_timeout_secs (default 300)",
        "reason:steer",
        "Ordinary steering does not release foreground waits",
        "background:true returns immediately with a running handle",
        "timeoutMs is a hard deadline that kills",
    ] {
        assert!(
            description("exec").contains(phrase),
            "exec discovery lacks {phrase:?}"
        );
    }
    assert!(description("wait").contains("timeout without cancelling the process"));
    assert!(description("poll").contains("Pass a cursor for an independent reader"));
    assert!(description("jobs").contains("cannot be recovered after restart"));
    // The superseded contract said yieldMs defaulted to 1000 with a 30000 cap.
    for stale in ["yieldms defaults to 1000", "max 30000"] {
        assert!(!js.contains(stale), "js reference still says {stale:?}");
    }
    f.close().await;
}
