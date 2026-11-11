use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn missing_client_variable_fails_as_standalone_despite_host_value() {
    let lab = Lab::start().await;
    let prompt = echo_prompt("mcp__missing__echo", "missing-session");

    // Standalone: no owner, its own data directory, and no variable anywhere.
    let mut standalone = Proc::spawn(
        lab.root(),
        "standalone",
        lab.command(
            &lab.standalone_config,
            "standalone",
            &[],
            &["run", prompt.as_str()],
        ),
    );
    let status = standalone.finish().await;
    assert!(status.success(), "{}", standalone.output());
    assert!(
        standalone.output().contains(MISSING_MESSAGE),
        "{}",
        standalone.output()
    );

    // Attached: the owner has the variable; the client does not.
    let data = lab.data();
    let mut owner = lab.start_owner(&data, &[(MISSING_VAR, HOST_MISSING)]).await;
    let mut client = Proc::spawn(
        lab.root(),
        "attached",
        lab.command(&lab.config, "attached", &[], &["run", prompt.as_str()]),
    );
    let status = client.finish().await;
    assert!(status.success(), "{}", client.output());
    assert!(
        client.output().contains(MISSING_MESSAGE),
        "{}",
        client.output()
    );

    // The missing server never receives a call, so the host value cannot reach it.
    assert!(
        lab.http_calls()
            .iter()
            .all(|call| call["text"] != "missing-session"),
        "{:?}",
        lab.http_calls()
    );
    lab.stop_owner(&mut owner).await;

    let hits = lab.secret_hits(&[HOST_MISSING]);
    assert!(hits.is_empty(), "host value leaked: {hits:?}");
}
