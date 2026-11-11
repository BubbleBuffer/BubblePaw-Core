//! Framing parity with the protocol as it is: `stdio` and `POST /rpc` both go
//! through `host_api::rpc::handle_bytes`, so their replies must match exactly.
//! A reply to a request that cannot be read carries a null id. A notification
//! gets no reply (HTTP 204 on `/rpc`). An envelope that is not a valid shutdown
//! must not stop the process, and a request after the bad frames still gets
//! its own reply.
use super::{
    normalize,
    peer::{self, Peer, Transport},
};
use serde_json::{Value, json};
use std::path::Path;

struct Frame {
    name: &'static str,
    bytes: &'static [u8],
    /// Notifications carry no id and must produce no reply.
    answer: bool,
    /// The JSON-RPC error code a reply must carry; None when there is no error.
    code: Option<i64>,
    /// The id the reply must echo. A request that cannot be read gets null.
    id: Value,
    /// Text the error message must contain.
    message: Option<&'static str>,
}

fn frames() -> Vec<Frame> {
    vec![
        Frame {
            name: "parse error",
            bytes: b"{not json",
            answer: true,
            code: Some(-32700),
            id: Value::Null,
            message: Some("invalid JSON"),
        },
        Frame {
            name: "invalid request",
            bytes: br#"{"jsonrpc":"1.0","id":7,"method":"initialize","params":{}}"#,
            answer: true,
            code: Some(-32600),
            id: Value::Null,
            message: Some("invalid request"),
        },
        Frame {
            name: "unknown method",
            bytes: br#"{"jsonrpc":"2.0","id":8,"method":"no.such.method","params":{}}"#,
            answer: true,
            code: Some(-32000),
            id: json!(8),
            message: Some("unknown method"),
        },
        Frame {
            name: "malformed shutdown",
            bytes: br#"{"jsonrpc":"1.0","id":9,"method":"shutdown","params":{}}"#,
            answer: true,
            code: Some(-32600),
            id: Value::Null,
            message: Some("invalid request"),
        },
        Frame {
            name: "notification",
            bytes: br#"{"jsonrpc":"2.0","method":"initialize","params":{}}"#,
            answer: false,
            code: None,
            id: Value::Null,
            message: None,
        },
    ]
}

/// Both transports run on one data directory in turn, so their paths and ids
/// match and any difference is in the framing alone.
#[tokio::test]
async fn stdio_and_rpc_frame_the_same_way() {
    let root = tempfile::tempdir().unwrap();
    peer::seed(root.path());
    let stdio = outcomes(Transport::Stdio, root.path(), "stdio").await;
    let rpc = outcomes(Transport::Rpc, root.path(), "http").await;
    assert_eq!(stdio, rpc, "stdio and /rpc frame differently");
}

async fn outcomes(transport: Transport, root: &Path, role: &str) -> Vec<Value> {
    let mut peer = Peer::start(transport, root).await;
    let mut outcomes = Vec::new();
    for frame in frames() {
        let reply = peer.raw(frame.bytes, frame.answer).await;
        check(&frame, reply.as_ref(), transport);
        let shown = reply.unwrap_or(Value::Null);
        outcomes.push(json!({
            "frame": frame.name,
            "reply": normalize::response(&shown, role),
        }));
    }
    // The malformed shutdown must not have ended the process: a request after
    // the bad frames is answered with its own id.
    let after = peer.call("initialize", json!({})).await;
    assert!(
        after.get("error").is_none(),
        "{transport:?}: initialize after bad frames failed: {after}"
    );
    outcomes.push(json!({"frame": "after", "reply": normalize::response(&after, role)}));
    peer.call("shutdown", json!({})).await;
    peer.finish().await;
    outcomes
}

fn check(frame: &Frame, reply: Option<&Value>, transport: Transport) {
    if !frame.answer {
        assert!(
            reply.is_none(),
            "{transport:?} {}: a notification was answered: {reply:?}",
            frame.name
        );
        return;
    }
    let reply = reply.unwrap_or_else(|| panic!("{transport:?} {}: no reply", frame.name));
    assert_eq!(
        reply["id"], frame.id,
        "{transport:?} {}: reply id: {reply}",
        frame.name
    );
    assert_eq!(
        reply["error"]["code"].as_i64(),
        frame.code,
        "{transport:?} {}: error code: {reply}",
        frame.name
    );
    if let Some(message) = frame.message {
        let got = reply["error"]["message"].as_str().unwrap_or_default();
        assert!(
            got.contains(message),
            "{transport:?} {}: expected message containing {message:?}, got {reply}",
            frame.name
        );
    }
}
