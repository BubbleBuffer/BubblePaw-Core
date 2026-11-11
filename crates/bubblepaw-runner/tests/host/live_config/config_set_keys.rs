//! A refused `config.set` names the leaf it is about in `data.key`, and leaves
//! `data` out when no one leaf is to blame.
use super::{config_set::setup, *};
use bubblepaw_runner::{
    config_set::ConfigSetError,
    host_api::{HostCtx, Profile, rpc},
};

/// The refusal a whole-entry write of `entry` on server `a` gets.
async fn refused_entry(fixture: &Fixture, entry: serde_json::Value) -> ConfigSetError {
    setup(fixture)
        .config_set("mcp.servers.a", Some(&entry))
        .await
        .expect_err("the entry is refused")
        .downcast::<ConfigSetError>()
        .expect("a typed config error")
}

#[tokio::test]
async fn an_entry_write_names_the_failing_leaf() {
    let fixture = Fixture::new(&config("")).await;
    let cases = [
        (json!({"url": "http://example.com/mcp"}), "url"),
        (json!({"url": "not a url"}), "url"),
        (json!({"url": "https://example.com/mcp?q=1"}), "url"),
        (json!({}), "command"),
        (json!({"enabled": true}), "command"),
        (json!({"command": ""}), "command"),
        (
            json!({"command": "x", "url": "https://example.com/mcp"}),
            "url",
        ),
        (
            json!({"command": "x", "call_timeout_secs": 0}),
            "call_timeout_secs",
        ),
        (
            json!({"command": "x", "startup_timeout_secs": 0}),
            "startup_timeout_secs",
        ),
        (
            json!({"command": "x", "headers_from": {"X-Key": "API_KEY"}}),
            "headers_from",
        ),
        (json!({"command": "x", "oauth": true}), "oauth"),
        (
            json!({"url": "https://example.com/mcp", "headers_from": {"X-Key": "not_allowed"}}),
            "headers_from",
        ),
        (json!({"command": "x", "env": {"A=B": "1"}}), "env"),
    ];
    for (entry, leaf) in cases {
        let error = refused_entry(&fixture, entry.clone()).await;
        assert_eq!(error.kind(), "invalid_value", "{entry}");
        assert_eq!(
            error.key(),
            Some(format!("mcp.servers.a.{leaf}").as_str()),
            "{entry}: {error}"
        );
    }
    fixture.shutdown().await;
}

#[tokio::test]
async fn a_refusal_no_one_leaf_caused_has_no_key() {
    let fixture = Fixture::new(&config("")).await;
    for entry in [
        json!({"command": 5}),
        json!({"command": "x", "no_such_field": 1}),
    ] {
        let error = refused_entry(&fixture, entry.clone()).await;
        assert_eq!(error.key(), None, "{entry}: {error}");
    }
    // A section is not a leaf either.
    let error = setup(&fixture)
        .config_set("agents", Some(&json!(1)))
        .await
        .unwrap_err()
        .downcast::<ConfigSetError>()
        .unwrap();
    assert_eq!(error.key(), None);
    fixture.shutdown().await;
}

#[tokio::test]
async fn a_leaf_write_names_its_own_key_over_rpc_and_the_message_is_unchanged() {
    let fixture = Fixture::new(&config("[mcp.servers.a]\ncommand='x'\n")).await;
    let mut ctx = HostCtx::for_store(fixture.store.clone(), fixture._root.path().to_owned());
    ctx.providers = Some(setup(&fixture));
    ctx.live_config = Some(fixture.live.clone());
    ctx.provider_config = Some(fixture.providers.clone());
    let call = |params: serde_json::Value| {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "config.set", "params": params});
        let ctx = ctx.clone();
        async move { rpc::handle(&ctx, Profile::Full, request).await.unwrap() }
    };
    let wrong = call(json!({"key": "agents.max_depth", "value": "x"})).await;
    assert_eq!(wrong["error"]["code"], -32011);
    assert_eq!(wrong["error"]["data"], json!({"key": "agents.max_depth"}));
    let leaf = call(json!({"key": "mcp.servers.a.call_timeout_secs", "value": 0})).await;
    assert_eq!(leaf["error"]["code"], -32012);
    assert_eq!(
        leaf["error"]["data"],
        json!({"key": "mcp.servers.a.call_timeout_secs"})
    );
    // An entry write: same code, the field's key, and the same message text.
    let entry =
        call(json!({"key": "mcp.servers.a", "value": {"url": "http://example.com/mcp"}})).await;
    assert_eq!(entry["error"]["code"], -32012);
    assert_eq!(entry["error"]["data"], json!({"key": "mcp.servers.a.url"}));
    assert_eq!(
        entry["error"]["message"],
        "Invalid config value: mcp.servers.a: url must use https unless it points at this machine (localhost/127.0.0.1/::1)"
    );
    // No single culprit: no `data` at all.
    let loose = call(json!({"key": "mcp.servers.a", "value": {"command": 5}})).await;
    assert_eq!(loose["error"]["code"], -32012);
    assert!(loose["error"].get("data").is_none(), "{loose}");
    let unknown = call(json!({"key": "no.such", "value": 1})).await;
    assert_eq!(unknown["error"]["code"], -32010);
    assert!(unknown["error"].get("data").is_none(), "{unknown}");
    fixture.shutdown().await;
}
