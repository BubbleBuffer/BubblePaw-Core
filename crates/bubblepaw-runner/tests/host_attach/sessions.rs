use super::*;
use bubblepaw_runner::host_client::{ClientError, HostClient};

/// Host-API JSON-RPC sessions share the host context next to attached runs.
/// `shutdown` ends only its own session; the host and other sessions go on.
#[tokio::test]
async fn endpoint_serves_host_api_sessions_next_to_the_host_run() {
    let lab = Lab::new();
    let data = lab.data();
    let a = lab.dir("a");
    let mut host = start_host(&lab, &data, &a, "sleep 2000").await;
    assert_eq!(host_json(&data).unwrap()["protocol_version"], 3);

    let first = HostClient::attach(&data, WAIT).await.unwrap();
    let count = |list: &Value| list.as_array().unwrap().len();
    let before = count(&first.call("session.list", json!({})).await.unwrap());
    assert!(before >= 1, "the host's own run session is listed");
    let opened = first
        .call("session.open", json!({"title":"over the endpoint"}))
        .await
        .unwrap();
    assert_eq!(opened["title"], "over the endpoint");

    let second = HostClient::attach(&data, WAIT).await.unwrap();
    let listed = second.call("session.list", json!({})).await.unwrap();
    assert_eq!(count(&listed), before + 1, "{listed}");
    assert!(listed.to_string().contains(opened["id"].as_str().unwrap()));

    assert_eq!(
        first.call("shutdown", json!({})).await.unwrap(),
        json!({"stopped":true})
    );
    assert_eq!(
        first.call("session.list", json!({})).await,
        Err(ClientError::Disconnected)
    );
    second
        .call("session.list", json!({}))
        .await
        .expect("other sessions outlive one session's shutdown");
    assert!(
        host.running(),
        "a session's shutdown leaves the host running"
    );
    first.close().await;
    second.close().await;
    stop_host(&lab, &mut host).await;
}
