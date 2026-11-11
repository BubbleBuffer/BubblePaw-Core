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
    assert_eq!(host_json(&data).unwrap()["protocol_version"], 4);

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

/// Open a raw `attach.session` with `params`; returns the reply and the
/// connection's halves.
async fn raw_session(
    data: &Path,
    params: Value,
) -> (
    Value,
    tokio::io::BufReader<tokio::io::ReadHalf<bubblepaw_runner::host_transport::Connection>>,
    tokio::io::WriteHalf<bubblepaw_runner::host_transport::Connection>,
) {
    use tokio::io::AsyncWriteExt;
    let connection = bubblepaw_runner::host_transport::connect(data, WAIT)
        .await
        .unwrap();
    let (read, mut write) = tokio::io::split(connection);
    let mut lines = tokio::io::BufReader::new(read);
    let open = json!({"jsonrpc":"2.0","id":0,"method":"attach.session","params":params});
    write
        .write_all(format!("{open}\n").as_bytes())
        .await
        .unwrap();
    let reply = bubblepaw_runner::rpc_session::read_record_async(&mut lines)
        .await
        .unwrap()
        .unwrap();
    (serde_json::from_slice(&reply).unwrap(), lines, write)
}

/// `attach.session {profile}` limits the session like a remote device's
/// tunnel; an unknown profile is refused.
#[tokio::test]
async fn attach_session_profile_limits_its_methods() {
    use tokio::io::AsyncWriteExt;
    let lab = Lab::new();
    let data = lab.data();
    let a = lab.dir("a");
    let mut host = start_host(&lab, &data, &a, "sleep 2000").await;

    let (reply, _, _) = raw_session(&data, json!({"profile":"owner"})).await;
    assert_eq!(reply["error"]["code"], -32602, "{reply}");

    let (reply, mut lines, mut write) = raw_session(&data, json!({"profile":"read"})).await;
    assert_eq!(reply["result"]["protocol_version"], 4, "{reply}");
    let set = json!({"jsonrpc":"2.0","id":1,"method":"config.set",
        "params":{"key":"agents.wake_debounce_ms","value":77}});
    write
        .write_all(format!("{set}\n").as_bytes())
        .await
        .unwrap();
    let refused = loop {
        let record = bubblepaw_runner::rpc_session::read_record_async(&mut lines)
            .await
            .unwrap()
            .expect("a reply");
        let record: Value = serde_json::from_slice(&record).unwrap();
        if record["id"] == 1 {
            break record;
        }
    };
    assert_eq!(refused["error"]["code"], -32003, "{refused}");
    drop(write);
    stop_host(&lab, &mut host).await;
}
