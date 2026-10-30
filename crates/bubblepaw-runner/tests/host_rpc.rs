#![cfg(feature = "web")]
//! `POST /rpc` runs the shared host table: a request gets a JSON-RPC reply,
//! a notification gets no body, and sessions it opens show up on REST.
use anyhow::Result;
use bubblepaw_core::{Harness, HarnessConfig, Store, ToolRegistry};
use bubblepaw_runner::{
    provider::ScriptedProvider,
    web::{WebConfig, WebServices, serve_with_services},
};
use serde_json::{Value, json};
use std::sync::Arc;

async fn post_rpc(client: &reqwest::Client, base: &str, body: Value) -> Result<reqwest::Response> {
    Ok(client
        .post(format!("{base}/rpc"))
        .json(&body)
        .send()
        .await?)
}

#[tokio::test]
async fn rpc_requests_run_the_shared_table_and_notifications_get_no_body() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let assets = directory.path().join("dist");
    std::fs::create_dir(&assets)?;
    std::fs::write(assets.join("index.html"), "fixture")?;
    let store = Store::open_data_dir(directory.path().join("data"))?;
    let harness = Arc::new(Harness::new(
        store.clone(),
        Arc::new(ScriptedProvider::new("No RPC test dispatches a model turn")),
        ToolRegistry::default(),
        HarnessConfig::default(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let (stop, receive) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(serve_with_services(
        listener,
        harness,
        store,
        WebConfig {
            workspace: directory.path().display().to_string(),
            model: "scripted".into(),
            provider: "demo".into(),
            read_only: false,
        },
        assets,
        WebServices::default(),
        async {
            let _ = receive.await;
        },
    ));
    let client = reqwest::Client::new();

    // A notification has no id, so the host sends no reply.
    let notification = post_rpc(
        &client,
        &base,
        json!({"jsonrpc": "2.0", "method": "config.get", "params": {}}),
    )
    .await?;
    assert_eq!(notification.status(), reqwest::StatusCode::NO_CONTENT);
    assert!(notification.bytes().await?.is_empty());

    // A request gets the same result the stdio transport would return.
    let config: Value = post_rpc(
        &client,
        &base,
        json!({"jsonrpc": "2.0", "id": 1, "method": "config.get", "params": {}}),
    )
    .await?
    .error_for_status()?
    .json()
    .await?;
    assert_eq!(config["id"], 1);
    assert_eq!(config["result"]["model"], "scripted");
    assert_eq!(config["result"]["session_limit"], 64);

    // A session opened over RPC is the one REST lists.
    let opened: Value = post_rpc(
        &client,
        &base,
        json!({"jsonrpc": "2.0", "id": "open", "method": "session.open", "params": {"title": "RPC session"}}),
    )
    .await?
    .error_for_status()?
    .json()
    .await?;
    assert_eq!(opened["id"], "open");
    let session_id = opened["result"]["id"].as_str().unwrap().to_owned();
    let listed: Value = client
        .get(format!("{base}/api/sessions"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["id"] == session_id)
    );

    // An unknown method is a JSON-RPC error with the request id.
    let unknown: Value = post_rpc(
        &client,
        &base,
        json!({"jsonrpc": "2.0", "id": 2, "method": "no.such", "params": {}}),
    )
    .await?
    .json()
    .await?;
    assert_eq!(unknown["id"], 2);
    assert_eq!(unknown["error"]["code"], -32000);
    assert_eq!(unknown["error"]["message"], "unknown method: no.such");

    // Malformed JSON gets the parse error and a null id.
    let malformed = client
        .post(format!("{base}/rpc"))
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await?;
    let malformed: Value = malformed.json().await?;
    assert_eq!(malformed["id"], Value::Null);
    assert_eq!(malformed["error"]["code"], -32700);

    let _ = stop.send(());
    task.await??;
    Ok(())
}
